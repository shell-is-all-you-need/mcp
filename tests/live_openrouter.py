#!/usr/bin/env python3
"""Opt-in live MCP/OpenAI-compatible tests with no Python dependencies.

Usage:
  python3 tests/live_openrouter.py --llm
  python3 tests/live_openrouter.py --examples
  python3 tests/live_openrouter.py --models
  python3 tests/live_openrouter.py --live-tools
  python3 tests/live_openrouter.py --media
  python3 tests/live_openrouter.py --all

The script loads .env from the workspace root, then the repository root,
without overriding existing process environment variables. It never prints
OPENAI_API_KEY.

Modes:
  --llm      single-model round trips using the .env OPENAI_MODEL
  --examples smoke-test every exact tool definition in mcp.example.json
  --models   multi-step scenario suite across every LIVE_MODELS model
  --live-tools real-model calls through all exact mcp.example.json tools
  --all      run --llm and --examples together
  --media    run live Reddit, image edit, image comparison and image generation

The multi-model suite runs each scenario against every model in LIVE_MODELS
(comma separated) or, when unset, the DEFAULT_MODELS list below. It prints a
per-model PASS/FAIL summary with elapsed seconds and exits non-zero when any
scenario fails.
"""

from __future__ import annotations

import argparse
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
TASK_EXTENSION = "io.modelcontextprotocol/tasks"
PROTOCOL_VERSION = "2026-07-28"

# Default model roster for `--models`. Override with the LIVE_MODELS
# environment variable (comma separated). The roster is intentionally
# test-only: production code has no model or vendor knowledge.
DEFAULT_MODELS = [
    "meta/muse-spark-1.3-contributor",
    "google/gemma-4-26b-a4b-it",
    "inclusionai/ling-3.0-flash",
    "z-ai/glm-5.3-flash",
    "deepseek/deepseek-v4-flash-0731",
    "qwen/qwen3.7-flash",
]


def load_dotenv(path: Path) -> None:
    if not path.exists():
        return
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip()
        value = value.strip()
        if value[:1] == value[-1:] and value[:1] in {"'", '"'}:
            value = value[1:-1]
        if key and key not in os.environ:
            os.environ[key] = value


def require_env(*names: str) -> None:
    missing = [name for name in names if not os.environ.get(name)]
    if missing:
        raise RuntimeError("missing environment variables: " + ", ".join(missing))


def binary() -> Path:
    configured = os.environ.get("SHELL_IS_ALL_YOU_NEED_BIN")
    if configured:
        path = Path(configured).expanduser().resolve()
        if not path.is_file():
            raise RuntimeError(f"SHELL_IS_ALL_YOU_NEED_BIN does not exist: {path}")
        return path

    suffix = ".exe" if os.name == "nt" else ""
    path = ROOT / "target" / "debug" / f"shell-is-all-you-need{suffix}"
    if not path.exists():
        subprocess.run(["cargo", "build", "--quiet"], cwd=ROOT, check=True)
    return path.resolve()


def envelope(tasks: bool) -> dict[str, object]:
    capabilities: dict[str, object] = {}
    if tasks:
        capabilities["extensions"] = {TASK_EXTENSION: {}}
    return {
        "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientInfo": {
            "name": "shell-is-all-you-need-live-test",
            "version": "1",
        },
        "io.modelcontextprotocol/clientCapabilities": capabilities,
    }


class McpProcess:
    def __init__(self, args: list[str], cwd: Path) -> None:
        self.process = subprocess.Popen(
            [str(binary()), *args],
            cwd=cwd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=None,
            text=True,
            encoding="utf-8",
            bufsize=1,
            env=os.environ.copy(),
        )
        assert self.process.stdin is not None
        assert self.process.stdout is not None
        self._next_id = 1

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)

    def __enter__(self) -> "McpProcess":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def request(self, method: str, params: dict[str, object], *, tasks: bool = False) -> dict[str, object]:
        request_id = self._next_id
        self._next_id += 1
        params = dict(params)
        params["_meta"] = envelope(tasks)
        message = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        self.process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.process.stdin.flush()
        line = self.process.stdout.readline()
        if not line:
            code = self.process.poll()
            raise RuntimeError(f"MCP server closed stdout unexpectedly (exit={code})")
        response = json.loads(line)
        if response.get("id") != request_id:
            raise RuntimeError(f"unexpected MCP response id: {response!r}")
        if "error" in response:
            raise RuntimeError(f"MCP error for {method}: {response['error']!r}")
        return response

    def malformed_then_discover(self) -> None:
        """Verify malformed input receives an error and the process remains usable."""
        self.process.stdin.write("{not-json}\n")
        self.process.stdin.flush()
        line = self.process.stdout.readline()
        response = json.loads(line)
        if "error" not in response:
            raise RuntimeError(f"malformed MCP input was not rejected: {response!r}")
        self.discover()

    def discover(self, *, tasks: bool = False) -> dict[str, object]:
        return self.request("server/discover", {}, tasks=tasks)["result"]

    def tools(self, *, tasks: bool = False) -> list[dict[str, object]]:
        result = self.request("tools/list", {}, tasks=tasks)["result"]
        tools = result.get("tools", [])
        if not isinstance(tools, list) or not tools:
            raise RuntimeError(f"expected at least one MCP tool, got {tools!r}")
        return tools

    def tool(self, name: str | None = None, *, tasks: bool = False) -> dict[str, object]:
        tools = self.tools(tasks=tasks)
        if name is None:
            if len(tools) != 1:
                raise RuntimeError(f"tool name required for multi-tool server: {tools!r}")
            return tools[0]
        for definition in tools:
            if definition.get("name") == name:
                return definition
        raise RuntimeError(f"MCP tool {name!r} was not advertised: {tools!r}")

    def call(self, name: str, arguments: dict[str, object], *, tasks: bool = False) -> dict[str, object]:
        result = self.request(
            "tools/call", {"name": name, "arguments": arguments}, tasks=tasks
        )["result"]
        if result.get("resultType") != "task":
            return result

        self.acknowledge_task(result)
        return self.wait_task(result)

    def acknowledge_task(self, task: dict[str, object]) -> None:
        """Exercise tasks/update for a newly created task."""
        task_id = task["taskId"]
        self.request(
            "tasks/update",
            {"taskId": task_id, "inputResponses": {}},
            tasks=True,
        )

    def wait_task(self, task: dict[str, object]) -> dict[str, object]:
        """Poll a previously created task without conflating its handle with its result."""
        task_id = task["taskId"]
        poll_ms = int(task.get("pollIntervalMs", 50))
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            state = self.request("tasks/get", {"taskId": task_id}, tasks=True)["result"]
            status = state["status"]
            if status == "completed":
                return state["result"]
            if status == "failed":
                raise RuntimeError(f"MCP task failed: {state.get('error')!r}")
            if status == "cancelled":
                raise RuntimeError("MCP task was cancelled")
            time.sleep(max(poll_ms, 1) / 1000)
        self.request("tasks/cancel", {"taskId": task_id}, tasks=True)
        raise TimeoutError(f"MCP task {task_id} did not finish")

    def task_state(self, task: dict[str, object]) -> dict[str, object]:
        return self.request(
            "tasks/get", {"taskId": task["taskId"]}, tasks=True
        )["result"]


def structured(result: dict[str, object]) -> dict[str, object]:
    value = result.get("structuredContent")
    if not isinstance(value, dict):
        raise RuntimeError(f"missing structuredContent: {result!r}")
    expected = {
        "exitCode",
        "stdout",
        "stderr",
        "timedOut",
        "stdoutTruncated",
        "stderrTruncated",
    }
    if set(value) != expected:
        raise RuntimeError(f"structuredContent keys differ from outputSchema: {value!r}")
    return value


def openai_chat(
    messages: list[dict[str, object]],
    *,
    tools: list[dict[str, object]] | None = None,
    tool_choice: dict[str, object] | None = None,
    retries: int = 3,
) -> dict[str, object]:
    require_env("OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL")
    payload: dict[str, object] = {
        "model": os.environ["OPENAI_MODEL"],
        "temperature": 0,
        "messages": messages,
    }
    if tools is not None:
        payload["tools"] = tools
    if tool_choice is not None:
        payload["tool_choice"] = tool_choice

    request = urllib.request.Request(
        os.environ["OPENAI_BASE_URL"].rstrip("/") + "/chat/completions",
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Authorization": f"Bearer {os.environ['OPENAI_API_KEY']}",
            "Content-Type": "application/json",
            "Accept": "application/json",
            "User-Agent": "shell-is-all-you-need-live-test",
        },
        method="POST",
    )
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(request, timeout=180) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            body = error.read().decode("utf-8", "replace")
            # Transient upstream rate limits: retry a bounded number of times.
            if error.code == 429 and attempt + 1 < retries:
                time.sleep(2 * (attempt + 1))
                continue
            raise RuntimeError(
                f"OpenAI-compatible API returned HTTP {error.code}: {body}"
            ) from error


def assistant_message(response: dict[str, object]) -> dict[str, object]:
    choices = response.get("choices")
    if not isinstance(choices, list) or not choices:
        raise RuntimeError(f"model response has no choices: {response!r}")
    message = choices[0].get("message")
    if not isinstance(message, dict):
        raise RuntimeError(f"model response has no assistant message: {response!r}")
    return message


def content_text(message: dict[str, object]) -> str:
    content = message.get("content", "")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(
            item.get("text", "") for item in content if isinstance(item, dict)
        )
    return str(content)


def force_tool_call(
    mcp: McpProcess, prompt: str, *, tasks: bool = False
) -> tuple[dict[str, object], dict[str, object], str]:
    definition = mcp.tool(tasks=tasks)
    name = definition["name"]
    openai_tool = {
        "type": "function",
        "function": {
            "name": name,
            "description": definition.get("description", ""),
            "parameters": definition["inputSchema"],
        },
    }
    messages: list[dict[str, object]] = [{"role": "user", "content": prompt}]
    first = openai_chat(
        messages,
        tools=[openai_tool],
        tool_choice={"type": "function", "function": {"name": name}},
    )
    assistant = assistant_message(first)
    calls = assistant.get("tool_calls")
    if not isinstance(calls, list) or len(calls) != 1:
        raise RuntimeError(f"model did not produce exactly one tool call: {assistant!r}")
    call = calls[0]
    function = call.get("function", {})
    raw_arguments = function.get("arguments", "{}")
    arguments = raw_arguments if isinstance(raw_arguments, dict) else json.loads(raw_arguments)
    result = mcp.call(name, arguments, tasks=tasks)
    tool_text = result.get("content", [{}])[0].get("text", "")
    assistant_history = {
        key: assistant[key]
        for key in ("role", "content", "tool_calls")
        if key in assistant
    }
    messages.extend(
        [
            assistant_history,
            {
                "role": "tool",
                "tool_call_id": call["id"],
                "content": tool_text,
            },
        ]
    )
    return result, {"messages": messages}, tool_text


def test_openai_connection() -> None:
    response = openai_chat(
        [{"role": "user", "content": "Reply with exactly LIVE_MODEL_OK."}]
    )
    text = content_text(assistant_message(response)).strip()
    if "LIVE_MODEL_OK" not in text:
        raise RuntimeError(f"unexpected live model response: {text!r}")
    print("PASS openai_connection")


def shell_server(cwd: Path) -> McpProcess:
    """Single-tool MCP server exposing `shell` (sh -c '{command}')."""
    schema = {
        "type": "object",
        "properties": {"command": {"type": "string"}},
        "required": ["command"],
        "additionalProperties": False,
    }
    args = [
        "--tool",
        "--name",
        "shell",
        "--description",
        "Execute the exact test shell command requested by the user.",
        "--input-schema",
        json.dumps(schema, separators=(",", ":")),
        "--exec",
        "sh",
        "-c",
        "{command}",
    ]
    return McpProcess(args, cwd)


def two_file_server(cwd: Path) -> McpProcess:
    """One MCP server exposing independently constrained `write` and `read`."""
    write_schema = {
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "content": {"type": "string"},
        },
        "required": ["path", "content"],
        "additionalProperties": False,
    }
    write_args = [
        "--tool",
        "--name",
        "write",
        "--description",
        "Create or completely replace a UTF-8 file.",
        "--input-schema",
        json.dumps(write_schema, separators=(",", ":")),
        "--fs-path-field",
        "path",
        "--fs-root",
        ".",
        "--fs-deny-path",
        ".env",
        "--exec",
        "python3",
        "-c",
        "from pathlib import Path; import sys; p=Path(sys.argv[1]); p.parent.mkdir(parents=True, exist_ok=True); p.write_text(sys.argv[2])",
        "{path}",
        "{content}",
    ]
    read_schema = {
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"],
        "additionalProperties": False,
    }
    read_args = [
        "--tool",
        "--name",
        "read",
        "--description",
        "Print a UTF-8 file.",
        "--input-schema",
        json.dumps(read_schema, separators=(",", ":")),
        "--fs-path-field",
        "path",
        "--fs-root",
        ".",
        "--fs-deny-path",
        ".env",
        "--exec",
        "sh",
        "-c",
        'cat -- "$1"',
        "shell-is-all-you-need:read",
        "{path}",
    ]
    return McpProcess([*write_args, *read_args], cwd)


def task_server(cwd: Path) -> McpProcess:
    """Task-enabled MCP server backed by an inline deterministic mock process."""
    schema = {
        "type": "object",
        "properties": {"message": {"type": "string"}},
        "required": ["message"],
        "additionalProperties": False,
    }
    delay_ms = os.environ.get("LIVE_TASK_TEST_DELAY_MS", "50")
    args = [
        "--tool",
        "--name",
        "task",
        "--description",
        "Run a deterministic mock task and return the requested message.",
        "--input-schema",
        json.dumps(schema, separators=(",", ":")),
        "--task-store-dir",
        str(cwd / "tasks"),
        "--task-poll-interval-ms",
        "50",
        "--process-timeout-ms",
        "120000",
        "--exec",
        sys.executable,
        "-c",
        "import sys,time; print('working...', file=sys.stderr, flush=True); time.sleep(int(sys.argv[1]) / 1000); print(sys.argv[2])",
        delay_ms,
        "{message}",
    ]
    return McpProcess(args, cwd)


def tool_defs(servers: dict[str, McpProcess], tasks: set[str] = frozenset()) -> list[dict[str, object]]:
    """OpenAI function definitions for every server tool already advertised."""
    definitions = []
    for name in servers:
        definition = servers[name].tool(name, tasks=name in tasks)
        definitions.append(
            {
                "type": "function",
                "function": {
                    "name": definition["name"],
                    "description": definition.get("description", ""),
                    "parameters": definition["inputSchema"],
                },
            }
        )
    return definitions


def _tool_choice_rejected(error: Exception) -> bool:
    """True when a provider refuses the requested tool_choice value."""
    return "tool_choice" in str(error)


def forced_step(
    messages: list[dict[str, object]],
    servers: dict[str, McpProcess],
    name: str,
    prompt: str,
    tools: list[dict[str, object]],
    tasks: set[str] = frozenset(),
    attempts: int = 3,
) -> tuple[dict[str, object], str]:
    """Steer exactly one call of `name`, route it to its MCP server, and append
    the assistant turn plus the tool result so the next prompt can build on it.

    Providers differ in tool_choice support: named forcing works on some,
    `required` on others, and a few only accept `auto`. The call therefore
    descends that ladder, and if the model answers with reasoning but no usable
    tool call, the attempt is rolled back and retried with a stronger
    instruction.
    """
    base_len = len(messages)
    choices: tuple[object, ...] = (
        {"type": "function", "function": {"name": name}},
        "required",
        "auto",
    )
    last_call_error: Exception | None = None
    for attempt in range(attempts):
        hint = prompt
        if attempt >= 1:
            hint += (
                f"\nIMPORTANT: you MUST call the {name} tool exactly once now "
                "with valid arguments. Reply only with that tool call; do not "
                "narrate or think out loud."
            )
        messages.append({"role": "user", "content": hint})

        first = None
        for choice in choices:
            try:
                first = openai_chat(messages, tools=tools, tool_choice=choice)  # type: ignore[arg-type]
                break
            except RuntimeError as error:
                last_call_error = error
                if not _tool_choice_rejected(error):
                    raise
        if first is None:
            raise RuntimeError(
                f"no supported tool_choice for {name!r}: {last_call_error}"
            )

        assistant = assistant_message(first)
        calls = assistant.get("tool_calls")
        if (
            not isinstance(calls, list)
            or len(calls) != 1
            or calls[0].get("function", {}).get("name") != name
        ):
            # Discard the failed attempt's prompt and assistant turn.
            del messages[base_len:]
            continue
        call = calls[0]
        function = call.get("function", {})
        raw_arguments = function.get("arguments", "{}")
        arguments = raw_arguments if isinstance(raw_arguments, dict) else json.loads(raw_arguments)
        result = servers[name].call(name, arguments, tasks=name in tasks)
        text = result.get("content", [{}])[0].get("text", "")
        messages.append(
            {key: assistant[key] for key in ("role", "content", "tool_calls") if key in assistant}
        )
        messages.append({"role": "tool", "tool_call_id": call["id"], "content": text})
        return result, text

    raise RuntimeError(
        f"model did not produce exactly one {name} call after {attempts} attempt(s)"
    )


def final_answer(messages: list[dict[str, object]], prompt: str) -> str:
    """Ask a final, non-forced question that must consume earlier tool results."""
    messages.append({"role": "user", "content": prompt})
    followup = openai_chat(messages)
    return content_text(assistant_message(followup)).strip()


def scenario_multi_step_shell() -> None:
    """Write a file via shell, read it back through a second forced call, then
    require the model to echo the content returned by the second call."""
    marker = "MULTI_STEP_ALPHA"
    with tempfile.TemporaryDirectory(prefix="mcp-scenario-chain-") as raw:
        cwd = Path(raw)
        with shell_server(cwd) as server:
            servers = {"shell": server}
            tools = tool_defs(servers)
            messages: list[dict[str, object]] = []

            result, _ = forced_step(
                messages,
                servers,
                "shell",
                f"Call shell with exactly this command value: printf '%s' '{marker}' > a.txt",
                tools,
            )
            if structured(result)["exitCode"] != 0:
                raise RuntimeError(f"step 1 shell failed: {result!r}")

            verify = server.call("shell", {"command": "cat a.txt"})
            if marker not in structured(verify)["stdout"]:
                raise RuntimeError(f"step 1 file missing: {verify!r}")

            _, text = forced_step(
                messages, servers, "shell", "Call shell with exactly this command value: cat a.txt", tools
            )
            if marker not in text:
                raise RuntimeError(f"step 2 did not surface file content: {text!r}")

            final = final_answer(
                messages, "Reply with exactly the content returned by your last tool call."
            )
            if marker not in final:
                raise RuntimeError(f"model did not consume chained result: {final!r}")

    print("PASS scenario_multi_step_shell")


def scenario_error_recovery() -> None:
    """First call must fail (exit 3). The model must then recover with a second
    call and prove the recovery on disk."""
    with tempfile.TemporaryDirectory(prefix="mcp-scenario-recover-") as raw:
        cwd = Path(raw)
        with shell_server(cwd) as server:
            servers = {"shell": server}
            tools = tool_defs(servers)
            messages: list[dict[str, object]] = []

            result, _ = forced_step(
                messages, servers, "shell", "Call shell with exactly this command value: exit 3", tools
            )
            values = structured(result)
            if result.get("isError") is not True or values["exitCode"] != 3:
                raise RuntimeError(f"expected failing first step: {result!r}")

            result, _ = forced_step(
                messages,
                servers,
                "shell",
                "Your previous tool call failed. Recover by calling shell with exactly this command value: printf '%s' 'RECOVERED_OK' > recovered.txt",
                tools,
            )
            if structured(result)["exitCode"] != 0:
                raise RuntimeError(f"recovery call failed: {result!r}")

            verify = server.call("shell", {"command": "cat recovered.txt"})
            if "RECOVERED_OK" not in structured(verify)["stdout"]:
                raise RuntimeError(f"recovery file missing: {verify!r}")

            final = final_answer(messages, "Reply with exactly RECOVERED_OK.")
            if "RECOVERED_OK" not in final:
                raise RuntimeError(f"model did not close the recovery loop: {final!r}")

    print("PASS scenario_error_recovery")


def scenario_two_tool_chain() -> None:
    """Call two tools advertised by one MCP process in one conversation."""
    marker = "CHAIN_TOKEN_OMEGA"
    with tempfile.TemporaryDirectory(prefix="mcp-scenario-twotool-") as raw:
        cwd = Path(raw)
        with two_file_server(cwd) as mcp:
            advertised = [tool["name"] for tool in mcp.tools()]
            if advertised != ["write", "read"]:
                raise RuntimeError(f"same-server tools/list mismatch: {advertised!r}")
            servers = {"write": mcp, "read": mcp}
            tools = tool_defs(servers)
            messages: list[dict[str, object]] = []

            result, _ = forced_step(
                messages,
                servers,
                "write",
                f'Call write with path "chain.txt" and content exactly "{marker}".',
                tools,
            )
            if result.get("isError") is True:
                raise RuntimeError(f"write step failed: {result!r}")

            verify = mcp.call("read", {"path": "chain.txt"})
            if marker not in structured(verify)["stdout"]:
                raise RuntimeError(f"write was not durable: {verify!r}")

            _, text = forced_step(
                messages, servers, "read", 'Call read with path "chain.txt".', tools
            )
            if marker not in text:
                raise RuntimeError(f"read step did not surface content: {text!r}")

            final = final_answer(
                messages, "Reply with exactly the file content returned by your read tool call."
            )
            if marker not in final:
                raise RuntimeError(f"model did not consume two-tool result: {final!r}")

    print("PASS scenario_two_tool_chain")


def scenario_task_then_continue() -> None:
    """Start a durable task, do ordinary shell work, then consume the result."""
    marker = "TASK_TOKEN_BRAVO"
    with tempfile.TemporaryDirectory(prefix="mcp-scenario-task-") as raw:
        cwd = Path(raw)
        previous_delay = os.environ.get("LIVE_TASK_TEST_DELAY_MS")
        os.environ["LIVE_TASK_TEST_DELAY_MS"] = "3000"
        try:
            task_mcp = task_server(cwd)
        finally:
            if previous_delay is None:
                os.environ.pop("LIVE_TASK_TEST_DELAY_MS", None)
            else:
                os.environ["LIVE_TASK_TEST_DELAY_MS"] = previous_delay

        with task_mcp, shell_server(cwd) as shell_mcp:
            servers = {"task": task_mcp, "shell": shell_mcp}
            tools = tool_defs(servers, tasks={"task"})
            definition = next(
                item for item in tools if item["function"]["name"] == "task"
            )
            messages: list[dict[str, object]] = [
                {
                    "role": "user",
                    "content": f"Call task with message exactly {marker}.",
                }
            ]
            started_response = None
            last_choice_error: Exception | None = None
            for choice in (
                {"type": "function", "function": {"name": "task"}},
                "required",
                "auto",
            ):
                try:
                    started_response = openai_chat(
                        messages, tools=[definition], tool_choice=choice  # type: ignore[arg-type]
                    )
                    break
                except RuntimeError as error:
                    last_choice_error = error
                    if not _tool_choice_rejected(error):
                        raise
            if started_response is None:
                raise RuntimeError(f"no supported task tool_choice: {last_choice_error}")
            assistant = assistant_message(started_response)
            calls = assistant.get("tool_calls")
            if not isinstance(calls, list) or len(calls) != 1:
                raise RuntimeError(f"model did not start exactly one task: {assistant!r}")
            call = calls[0]
            raw_arguments = call.get("function", {}).get("arguments", "{}")
            arguments = (
                raw_arguments
                if isinstance(raw_arguments, dict)
                else json.loads(raw_arguments)
            )
            started = task_mcp.request(
                "tools/call",
                {"name": "task", "arguments": arguments},
                tasks=True,
            )["result"]
            if started.get("resultType") != "task":
                raise RuntimeError(f"expected task handle: {started!r}")
            task_mcp.acknowledge_task(started)
            if task_mcp.task_state(started)["status"] != "working":
                raise RuntimeError("delayed task was not working before concurrent model work")

            messages.append(
                {
                    key: assistant[key]
                    for key in ("role", "content", "tool_calls")
                    if key in assistant
                }
            )
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": call["id"],
                    "content": f"Task started with taskId {started['taskId']}.",
                }
            )

            result, _ = forced_step(
                messages,
                servers,
                "shell",
                "Call shell with exactly this command value: printf '%s' 'AFTER_TASK' > after.txt",
                tools,
            )
            if structured(result)["exitCode"] != 0:
                raise RuntimeError(f"post-task shell failed: {result!r}")

            verify = shell_mcp.call("shell", {"command": "cat after.txt"})
            if "AFTER_TASK" not in structured(verify)["stdout"]:
                raise RuntimeError(f"post-task file missing: {verify!r}")

            task_result = task_mcp.wait_task(started)
            task_text = task_result.get("content", [{}])[0].get("text", "")
            values = structured(task_result)
            if values["exitCode"] != 0 or marker not in task_text:
                raise RuntimeError(f"task result mismatch: {task_result!r}")

            final = final_answer(
                messages,
                f"Task {started['taskId']} has completed. Its result is:\n{task_text}\nReply with exactly {marker}.",
            )
            if marker not in final:
                raise RuntimeError(f"model did not consume task result: {final!r}")

    print("PASS scenario_task_then_continue")


# Scenarios run per model by the multi-model suite. A scenario is a plain
# function that raises on failure; the runner reports PASS/FAIL with timing.
MODEL_SCENARIOS: list[tuple[str, Any]] = [
    ("openai_connection", test_openai_connection),
    ("multi_step_shell", scenario_multi_step_shell),
    ("error_recovery", scenario_error_recovery),
    ("two_tool_chain", scenario_two_tool_chain),
    ("task_then_continue", scenario_task_then_continue),
]


def selected_models() -> list[str]:
    """Model roster for --models: LIVE_MODELS env override, else the default list."""
    raw = os.environ.get("LIVE_MODELS", "")
    names = [name.strip() for name in raw.replace(";", ",").split(",") if name.strip()]
    return names or list(DEFAULT_MODELS)


def run_multi_model_suite() -> int:
    """Run every scenario against every selected model. A failure in one
    scenario does not stop other scenarios or models."""
    require_env("OPENAI_API_KEY", "OPENAI_BASE_URL")
    models = selected_models()
    print(f"Multi-model scenario suite for {len(models)} model(s): {', '.join(models)}")
    results: list[tuple[str, str, str, float]] = []
    failures = 0
    for model in models:
        previous = os.environ.get("OPENAI_MODEL")
        os.environ["OPENAI_MODEL"] = model
        print(f"\n=== {model} ===")
        try:
            for name, scenario in MODEL_SCENARIOS:
                started = time.monotonic()
                try:
                    scenario()
                    elapsed = time.monotonic() - started
                    results.append((model, name, "PASS", elapsed))
                    print(f"  {name}: PASS ({elapsed:.1f}s)")
                except Exception as error:
                    elapsed = time.monotonic() - started
                    results.append((model, name, "FAIL", elapsed))
                    failures += 1
                    print(f"  {name}: FAIL ({elapsed:.1f}s): {error}", file=sys.stderr)
        finally:
            if previous is None:
                os.environ.pop("OPENAI_MODEL", None)
            else:
                os.environ["OPENAI_MODEL"] = previous
    print("\n=== Multi-model summary ===")
    print(f"{'model':<42} {'scenario':<22} {'result':<6} {'seconds':>8}")
    for model, name, status, elapsed in results:
        print(f"{model:<42} {name:<22} {status:<6} {elapsed:>7.1f}")
    print(f"\nscenario failures: {failures}")
    return failures


def test_llm_shell() -> None:
    if os.name == "nt":
        invocation = ["cmd", "/C", "{command}"]
        command = "echo SHELL_TOOL_OK"
    else:
        invocation = ["sh", "-c", "{command}"]
        command = "printf 'SHELL_TOOL_OK'"
    schema = {
        "type": "object",
        "properties": {"command": {"type": "string"}},
        "required": ["command"],
        "additionalProperties": False,
    }
    with tempfile.TemporaryDirectory(prefix="mcp-live-shell-") as raw:
        cwd = Path(raw)
        args = [
            "--tool",
        "--name",
            "shell",
            "--description",
            "Execute the exact safe test shell command requested by the user.",
            "--input-schema",
            json.dumps(schema, separators=(",", ":")),
            "--exec",
            *invocation,
        ]
        with McpProcess(args, cwd) as mcp:
            result, context, tool_text = force_tool_call(
                mcp,
                f"Call shell with exactly this command: {command}. After seeing the tool result, the final answer must be exactly SHELL_LLM_OK.",
            )
            if "SHELL_TOOL_OK" not in tool_text:
                raise RuntimeError(f"shell tool output mismatch: {result!r}")
            followup = openai_chat(context["messages"])
            final = content_text(assistant_message(followup)).strip()
            if "SHELL_LLM_OK" not in final:
                raise RuntimeError(f"model did not consume tool result: {final!r}")
    print("PASS llm_shell_round_trip")


def test_llm_durable_task() -> None:
    with tempfile.TemporaryDirectory(prefix="mcp-live-task-") as raw:
        cwd = Path(raw)
        with task_server(cwd) as mcp:
            discover = mcp.discover(tasks=True)
            extensions = discover["capabilities"].get("extensions", {})
            if TASK_EXTENSION not in extensions:
                raise RuntimeError("server/discover did not advertise Tasks")
            result, context, tool_text = force_tool_call(
                mcp,
                "Call task with message exactly TASK_TOOL_OK. After the task result arrives, the final answer must be exactly TASK_LLM_OK.",
                tasks=True,
            )
            values = structured(result)
            if values["exitCode"] != 0 or "TASK_TOOL_OK" not in tool_text:
                raise RuntimeError(f"task output mismatch: {result!r}")
            followup = openai_chat(context["messages"])
            final = content_text(assistant_message(followup)).strip()
            if "TASK_LLM_OK" not in final:
                raise RuntimeError(f"model did not consume task result: {final!r}")
    print("PASS llm_durable_task_round_trip")


def require_command(name: str) -> None:
    if os.path.isabs(name):
        if not Path(name).exists():
            raise RuntimeError(f"required executable is missing: {name}")
    elif shutil.which(name) is None:
        raise RuntimeError(f"required executable is missing from PATH: {name}")


def require_example_commands(args: list[str]) -> None:
    for index, value in enumerate(args[:-1]):
        if value == "--exec":
            require_command(args[index + 1])


def example_config() -> dict[str, object]:
    """Load the canonical examples; live adapters must never duplicate them."""
    return json.loads((ROOT / "mcp.example.json").read_text(encoding="utf-8"))


def exact_servers(stack: ExitStack, cwd: Path) -> tuple[dict[str, McpProcess], dict[str, tuple[McpProcess, str]]]:
    """Start each exact example server and map namespaced AI names to raw MCP tools."""
    config = example_config()
    servers: dict[str, McpProcess] = {}
    routes: dict[str, tuple[McpProcess, str]] = {}
    for namespace, server_config in config["servers"].items():
        args = list(server_config["args"])
        require_example_commands(args)
        server = stack.enter_context(McpProcess(args, cwd))
        servers[namespace] = server
        for definition in server.tools(tasks=namespace == "tasks"):
            raw_name = str(definition["name"])
            routes[f"{namespace}_{raw_name}"] = (server, raw_name)
    return servers, routes


def namespaced_tool_defs(routes: dict[str, tuple[McpProcess, str]]) -> list[dict[str, object]]:
    definitions = []
    for public_name, (server, raw_name) in routes.items():
        definition = server.tool(raw_name, tasks=public_name == "tasks_run")
        definitions.append(
            {
                "type": "function",
                "function": {
                    "name": public_name,
                    "description": definition.get("description", ""),
                    "parameters": definition["inputSchema"],
                },
            }
        )
    return definitions


def force_namespaced_call(
    routes: dict[str, tuple[McpProcess, str]],
    tools: list[dict[str, object]],
    public_name: str,
    prompt: str,
) -> tuple[dict[str, object], dict[str, object], list[dict[str, object]]]:
    """Require one exact namespaced model call, execute its raw MCP route, and retain history."""
    messages: list[dict[str, object]] = [{"role": "user", "content": prompt}]
    intended_tools = [tool for tool in tools if tool["function"]["name"] == public_name]
    if len(intended_tools) != 1:
        raise RuntimeError(f"{public_name}: missing or duplicate AI tool definition")
    response = openai_chat(
        messages,
        tools=intended_tools,
        tool_choice={"type": "function", "function": {"name": public_name}},
    )
    assistant = assistant_message(response)
    calls = assistant.get("tool_calls")
    if not isinstance(calls, list) or len(calls) != 1:
        raise RuntimeError(f"{public_name}: model did not emit exactly one tool call")
    call = calls[0]
    function = call.get("function", {})
    if function.get("name") != public_name:
        raise RuntimeError(f"{public_name}: model selected {function.get('name')!r}")
    raw_arguments = function.get("arguments", "{}")
    try:
        arguments = raw_arguments if isinstance(raw_arguments, dict) else json.loads(raw_arguments)
    except (TypeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"{public_name}: model arguments were not valid JSON") from error
    if not isinstance(arguments, dict):
        raise RuntimeError(f"{public_name}: model arguments were not an object")
    server, raw_name = routes[public_name]
    result = server.call(raw_name, arguments, tasks=public_name == "tasks_run")
    values = structured(result)
    if result.get("isError") is True or values["exitCode"] != 0:
        raise RuntimeError(f"{public_name}: raw MCP call failed: {result!r}")
    tool_text = result.get("content", [{}])[0].get("text", "")
    messages.extend(
        [
            {key: assistant[key] for key in ("role", "content", "tool_calls") if key in assistant},
            {"role": "tool", "tool_call_id": call["id"], "content": tool_text},
        ]
    )
    return result, arguments, messages


def assert_model_consumes(messages: list[dict[str, object]], marker: str, public_name: str) -> None:
    answer = final_answer(
        messages,
        f"Using the real {public_name} result above, reply with the exact marker {marker}.",
    )
    if marker not in answer:
        raise RuntimeError(f"{public_name}: follow-up did not consume the tool result")


def test_examples() -> None:
    config = example_config()
    expected_names = {
        "files": ["read", "write", "edit", "patch"],
        "search": ["glob", "grep"],
        "web": ["fetch", "search"],
        "shell": ["run"],
        "tasks": ["run"],
        "skills": ["list", "get"],
    }
    if list(config.get("servers", {})) != list(expected_names):
        raise RuntimeError(f"unexpected example server namespaces: {config!r}")

    require_command("patch")

    with tempfile.TemporaryDirectory(prefix="mcp-live-examples-") as raw:
        cwd = Path(raw)
        (cwd / "src").mkdir()
        (cwd / ".agents" / "skills" / "live").mkdir(parents=True)
        for invalid in (
            "bad-name",
            "mismatch",
            "missing-description",
            "huge-description",
            "unclosed-frontmatter",
        ):
            (cwd / ".agents" / "skills" / invalid).mkdir(parents=True)
        (cwd / "read.txt").write_text("READ_OK\n", encoding="utf-8")
        (cwd / "edit.txt").write_text("before\n", encoding="utf-8")
        (cwd / "edit-zero.txt").write_text("nothing here\n", encoding="utf-8")
        (cwd / "edit-many.txt").write_text("twice twice\n", encoding="utf-8")
        (cwd / "patch.txt").write_text("old\n", encoding="utf-8")
        (cwd / "src" / "live.rs").write_text("// TODO LIVE_GREP_OK\n", encoding="utf-8")
        (cwd / "src" / "unrelated.txt").write_text("not rust\n", encoding="utf-8")
        (cwd / ".env").write_text("EXCLUDED_SEARCH_MARKER\n", encoding="utf-8")
        (cwd / "env-link").symlink_to(cwd / ".env")
        (cwd / ".git").mkdir()
        (cwd / ".git" / "hidden").write_text("EXCLUDED_SEARCH_MARKER\n", encoding="utf-8")
        (cwd / ".mcp-tasks").mkdir()
        (cwd / ".mcp-tasks" / "hidden").write_text(
            "EXCLUDED_SEARCH_MARKER\n", encoding="utf-8"
        )
        outside = cwd.parent / f"{cwd.name}-outside.txt"
        outside.write_text("OUTSIDE\n", encoding="utf-8")
        (cwd / "escape-link").symlink_to(outside)
        (cwd / ".agents" / "skills" / "live" / "SKILL.md").write_text(
            "---\nname: live\ndescription: Deterministic live fixture.\n---\nSKILL_OK\n",
            encoding="utf-8",
        )
        (cwd / ".agents" / "skills" / "bad-name" / "SKILL.md").write_text(
            "---\nname: Bad Name\ndescription: Invalid uppercase and spaces.\n---\n",
            encoding="utf-8",
        )
        (cwd / ".agents" / "skills" / "mismatch" / "SKILL.md").write_text(
            "---\nname: other\ndescription: Name does not match directory.\n---\n",
            encoding="utf-8",
        )
        (cwd / ".agents" / "skills" / "missing-description" / "SKILL.md").write_text(
            "---\nname: missing-description\n---\n",
            encoding="utf-8",
        )
        (cwd / ".agents" / "skills" / "huge-description" / "SKILL.md").write_text(
            "---\nname: huge-description\ndescription: " + "x" * 1025 + "\n---\n",
            encoding="utf-8",
        )
        (cwd / ".agents" / "skills" / "unclosed-frontmatter" / "SKILL.md").write_text(
            "---\nname: unclosed-frontmatter\ndescription: Missing closing delimiter.\n",
            encoding="utf-8",
        )

        class FetchHandler(BaseHTTPRequestHandler):
            def do_GET(self) -> None:  # noqa: N802 - stdlib callback name
                body = b"WEB_FETCH_OK\n"
                self.send_response(200)
                self.send_header("Content-Type", "text/plain; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_: object) -> None:
                return

        fetch_server = ThreadingHTTPServer(("127.0.0.1", 0), FetchHandler)
        fetch_thread = threading.Thread(target=fetch_server.serve_forever, daemon=True)
        fetch_thread.start()

        # The public DuckDuckGo endpoint is intentionally not a CI dependency.
        # A tiny test-only urllib package verifies the exact URL constructed by the
        # production one-liner while keeping the MCP definition itself unchanged.
        hook = cwd / "python-hook" / "urllib"
        hook.mkdir(parents=True)
        (hook / "__init__.py").write_text("", encoding="utf-8")
        (hook / "parse.py").write_text(
            "def quote_plus(value):\n"
            "    if value != 'Model Context Protocol':\n"
            "        raise RuntimeError(f'unexpected search query: {value}')\n"
            "    return 'Model+Context+Protocol'\n",
            encoding="utf-8",
        )
        (hook / "request.py").write_text(
            "class Request:\n"
            "    def __init__(self, url, headers=None):\n"
            "        self.full_url = url\n"
            "        self.headers = headers or {}\n"
            "class _Response:\n"
            "    def read(self):\n"
            "        return b'WEB_SEARCH_OK\\n'\n"
            "def urlopen(request, timeout=None):\n"
            "    expected = 'https://lite.duckduckgo.com/lite/?q=Model+Context+Protocol&kl=en-us&kp=0'\n"
            "    if request.full_url != expected:\n"
            "        raise RuntimeError(f'unexpected DuckDuckGo URL: {request.full_url}')\n"
            "    return _Response()\n",
            encoding="utf-8",
        )
        previous_pythonpath = os.environ.get("PYTHONPATH")

        def validate_definitions(mcp: McpProcess, namespace: str, names: list[str], tasks: bool) -> None:
            discover = mcp.discover(tasks=tasks)
            if "tools" not in discover["capabilities"]:
                raise RuntimeError(f"{namespace}: tools capability missing")
            definitions = mcp.tools(tasks=tasks)
            actual_names = [definition["name"] for definition in definitions]
            if actual_names != names:
                raise RuntimeError(
                    f"{namespace}: expected tools {names!r}, got {actual_names!r}"
                )
            if any("outputSchema" not in definition for definition in definitions):
                raise RuntimeError(f"{namespace}: missing outputSchema")
            args = config["servers"][namespace]["args"]
            starts = [index for index, value in enumerate(args) if value == "--tool"]
            for index, definition in enumerate(definitions):
                end = starts[index + 1] if index + 1 < len(starts) else len(args)
                block = args[starts[index] : end]
                expected_description = block[block.index("--description") + 1]
                expected_schema = json.loads(block[block.index("--input-schema") + 1])
                if definition.get("description") != expected_description:
                    raise RuntimeError(f"{namespace}/{definition['name']}: description drift")
                if definition.get("inputSchema") != expected_schema:
                    raise RuntimeError(f"{namespace}/{definition['name']}: input schema drift")
                rejected = mcp.call(
                    str(definition["name"]), {"undeclared_property": True}, tasks=tasks
                )
                if rejected.get("isError") is not True:
                    raise RuntimeError(
                        f"{namespace}/{definition['name']}: closed schema accepted undeclared input"
                    )

        try:
            for namespace, names in expected_names.items():
                server = config["servers"][namespace]
                args = list(server["args"])
                require_example_commands(args)
                tasks = namespace == "tasks"

                if namespace == "web":
                    # Real urllib against a local HTTP server validates web_fetch.
                    with McpProcess(args, cwd) as mcp:
                        validate_definitions(mcp, namespace, names, tasks)
                        fetch_url = f"http://127.0.0.1:{fetch_server.server_port}/fixture"
                        fetched = structured(mcp.call("fetch", {"url": fetch_url}))
                        if fetched["exitCode"] != 0 or "WEB_FETCH_OK" not in str(fetched["stdout"]):
                            raise RuntimeError(f"web/fetch failed: {fetched!r}")
                        rejected = structured(mcp.call("fetch", {"url": "file:///etc/passwd"}))
                        if rejected["exitCode"] == 0:
                            raise RuntimeError(f"web/fetch accepted non-HTTP URL: {rejected!r}")
                        for invalid_url in ("ftp://example.com/file", "https:///missing-host"):
                            rejected = structured(mcp.call("fetch", {"url": invalid_url}))
                            if rejected["exitCode"] == 0:
                                raise RuntimeError(f"web/fetch accepted invalid URL: {invalid_url}")
                        mcp.malformed_then_discover()

                    # Re-spawn so child python3 inherits the deterministic urllib
                    # shim and validates the exact DuckDuckGo Lite URL.
                    os.environ["PYTHONPATH"] = str(hook.parent) + (
                        os.pathsep + previous_pythonpath if previous_pythonpath else ""
                    )
                    try:
                        with McpProcess(args, cwd) as mcp:
                            searched = structured(
                                mcp.call("search", {"query": "Model Context Protocol"})
                            )
                            if searched["exitCode"] != 0 or "WEB_SEARCH_OK" not in str(searched["stdout"]):
                                raise RuntimeError(f"web/search failed: {searched!r}")
                    finally:
                        if previous_pythonpath is None:
                            os.environ.pop("PYTHONPATH", None)
                        else:
                            os.environ["PYTHONPATH"] = previous_pythonpath

                    print(f"PASS example_{namespace}_{'_'.join(names)}")
                    continue

                with McpProcess(args, cwd) as mcp:
                    validate_definitions(mcp, namespace, names, tasks)

                    if namespace == "files":
                        read = structured(mcp.call("read", {"path": "read.txt"}))
                        if read["exitCode"] != 0 or "READ_OK" not in str(read["stdout"]):
                            raise RuntimeError(f"files/read failed: {read!r}")
                        directory = structured(mcp.call("read", {"path": "src"}))
                        directory_output = str(directory["stdout"])
                        if "src/live.rs" not in directory_output or "src/unrelated.txt" not in directory_output:
                            raise RuntimeError(f"files/read directory listing failed: {directory!r}")
                        write = structured(
                            mcp.call(
                                "write", {"path": "nested/write.txt", "content": "WRITE_OK π\n"}
                            )
                        )
                        if write["exitCode"] != 0 or (cwd / "nested/write.txt").read_bytes() != "WRITE_OK π\n".encode():
                            raise RuntimeError(f"files/write failed: {write!r}")
                        edit = structured(
                            mcp.call(
                                "edit",
                                {
                                    "path": "edit.txt",
                                    "old_text": "before",
                                    "new_text": "EDIT_OK",
                                },
                            )
                        )
                        if edit["exitCode"] != 0 or "EDIT_OK" not in (cwd / "edit.txt").read_text():
                            raise RuntimeError(f"files/edit failed: {edit!r}")
                        for path, old in (("edit-zero.txt", "absent"), ("edit-many.txt", "twice")):
                            rejected = structured(
                                mcp.call(
                                    "edit",
                                    {"path": path, "old_text": old, "new_text": "wrong"},
                                )
                            )
                            if rejected["exitCode"] == 0:
                                raise RuntimeError(f"files/edit accepted ambiguous count for {path}")
                        patch = structured(
                            mcp.call(
                                "patch",
                                {
                                    "path": "patch.txt",
                                    "patch": "--- patch.txt\n+++ patch.txt\n@@ -1 +1 @@\n-old\n+PATCH_OK\n",
                                },
                            )
                        )
                        if patch["exitCode"] != 0 or (cwd / "patch.txt").read_text() != "PATCH_OK\n":
                            raise RuntimeError(f"files/patch failed: {patch!r}")
                        conflict = structured(
                            mcp.call(
                                "patch",
                                {
                                    "path": "patch.txt",
                                    "patch": "--- patch.txt\n+++ patch.txt\n@@ -1 +1 @@\n-missing\n+wrong\n",
                                },
                            )
                        )
                        if conflict["exitCode"] == 0:
                            raise RuntimeError("files/patch accepted conflicting patch")

                        security_calls = [
                            ("read", {"path": ".env"}),
                            ("write", {"path": ".env", "content": "wrong"}),
                            ("edit", {"path": ".env", "old_text": "x", "new_text": "y"}),
                            ("patch", {"path": ".env", "patch": "invalid"}),
                            ("read", {"path": "../" + outside.name}),
                            ("write", {"path": "../escape.txt", "content": "wrong"}),
                            ("read", {"path": "escape-link"}),
                            ("write", {"path": "escape-link", "content": "wrong"}),
                            ("edit", {"path": "escape-link", "old_text": "OUTSIDE", "new_text": "wrong"}),
                            ("patch", {"path": "escape-link", "patch": "invalid"}),
                            ("read", {"path": ".git"}),
                            ("write", {"path": ".git/wrong", "content": "wrong"}),
                            ("edit", {"path": ".mcp-tasks/hidden", "old_text": "x", "new_text": "y"}),
                            ("patch", {"path": ".git/hidden", "patch": "invalid"}),
                            ("write", {"path": ".mcp-tasks/wrong", "content": "wrong"}),
                        ]
                        for tool_name, arguments in security_calls:
                            rejected = mcp.call(tool_name, arguments)
                            if rejected.get("isError") is not True:
                                raise RuntimeError(f"files/{tool_name} escaped path policy")
                        if outside.read_text(encoding="utf-8") != "OUTSIDE\n":
                            raise RuntimeError("symlink escape modified outside file")

                    elif namespace == "search":
                        glob = structured(mcp.call("glob", {"pattern": "./src/*.rs"}))
                        if glob["exitCode"] != 0 or "./src/live.rs" not in str(glob["stdout"]):
                            raise RuntimeError(f"search/glob failed: {glob!r}")
                        if "unrelated.txt" in str(glob["stdout"]):
                            raise RuntimeError(f"search/glob returned unrelated path: {glob!r}")
                        grep = structured(mcp.call("grep", {"pattern": "LIVE_GREP_OK"}))
                        if grep["exitCode"] != 0 or "LIVE_GREP_OK" not in str(grep["stdout"]):
                            raise RuntimeError(f"search/grep failed: {grep!r}")
                        excluded = structured(mcp.call("grep", {"pattern": "EXCLUDED_SEARCH_MARKER"}))
                        if excluded["exitCode"] == 0 or str(excluded["stdout"]).strip():
                            raise RuntimeError("search/grep surfaced a denied directory or .env")
                        escaped = structured(mcp.call("grep", {"pattern": "OUTSIDE"}))
                        if str(escaped["stdout"]).strip():
                            raise RuntimeError("search/grep followed a symlink outside the workspace")

                    elif namespace == "shell":
                        result = structured(
                            mcp.call("run", {"command": "printf 'SHELL_OK'"})
                        )
                        if result["exitCode"] != 0 or "SHELL_OK" not in str(result["stdout"]):
                            raise RuntimeError(f"shell/run failed: {result!r}")

                    elif namespace == "tasks":
                        if TASK_EXTENSION not in mcp.discover(tasks=True)["capabilities"].get("extensions", {}):
                            raise RuntimeError("tasks extension not advertised when configured")
                        started_at = time.monotonic()
                        started = mcp.request(
                            "tools/call",
                            {"name": "run", "arguments": {"delay_ms": 1200, "message": "TASK_OK"}},
                            tasks=True,
                        )["result"]
                        if started.get("resultType") != "task":
                            raise RuntimeError("tasks/run did not return task handle")
                        mcp.acknowledge_task(started)
                        if mcp.task_state(started)["status"] != "working":
                            raise RuntimeError("tasks/run intermediate state was not observable")
                        result = structured(mcp.wait_task(started))
                        if result["exitCode"] != 0 or "TASK_OK" not in str(result["stdout"]):
                            raise RuntimeError(f"tasks/run failed: {result!r}")
                        if "working..." not in str(result["stderr"]):
                            raise RuntimeError(f"tasks/run mock did not report progress: {result!r}")
                        if time.monotonic() - started_at < 1.0:
                            raise RuntimeError("tasks/run returned before its real delay")

                    elif namespace == "skills":
                        listed = structured(mcp.call("list", {}))
                        discovered = json.loads(str(listed["stdout"]))
                        if discovered != [
                            {
                                "id": "live",
                                "name": "live",
                                "description": "Deterministic live fixture.",
                            }
                        ]:
                            raise RuntimeError(f"skills/list did not discover fixture: {listed!r}")
                        diagnostics = str(listed["stderr"])
                        for invalid in (
                            "bad-name",
                            "mismatch",
                            "missing-description",
                            "huge-description",
                            "unclosed-frontmatter",
                        ):
                            if invalid not in diagnostics:
                                raise RuntimeError(
                                    f"skills/list missing diagnostic for {invalid}: {listed!r}"
                                )
                        loaded = structured(mcp.call("get", {"skill_id": "live"}))
                        if "SKILL_OK" not in str(loaded["stdout"]):
                            raise RuntimeError(f"skills/get did not load fixture: {loaded!r}")
                        for invalid_id in ("../live", "../../../../etc"):
                            rejected = mcp.call("get", {"skill_id": invalid_id})
                            if rejected.get("isError") is not True:
                                raise RuntimeError("skills/get accepted path escape")

                    mcp.malformed_then_discover()

                print(f"PASS example_{namespace}_{'_'.join(names)}")
        finally:
            fetch_server.shutdown()
            fetch_server.server_close()
            fetch_thread.join(timeout=5)
            if previous_pythonpath is None:
                os.environ.pop("PYTHONPATH", None)
            else:
                os.environ["PYTHONPATH"] = previous_pythonpath
            outside.unlink(missing_ok=True)


def test_public_web_examples() -> None:
    """Exercise both exact web tools against the real public network."""
    config = example_config()
    with tempfile.TemporaryDirectory(prefix="mcp-public-web-") as raw:
        with McpProcess(list(config["servers"]["web"]["args"]), Path(raw)) as mcp:
            fetched = structured(mcp.call("fetch", {"url": "https://example.com"}))
            if fetched["exitCode"] != 0 or not str(fetched["stdout"]).strip():
                raise RuntimeError("real web_fetch returned no content")
            searched = structured(
                mcp.call("search", {"query": os.environ.get("LIVE_SEARCH_QUERY", "Model Context Protocol")})
            )
            body = str(searched["stdout"])
            if searched["exitCode"] != 0 or not body.strip():
                raise RuntimeError("real DuckDuckGo Lite web_search returned no content")
            if "duckduckgo" not in body.lower() or "result" not in body.lower():
                raise RuntimeError("real web_search response did not resemble DuckDuckGo Lite results")
    print("PASS public_web_fetch_search")


def test_live_example_tools() -> None:
    """Have the configured real model call every exact, namespaced example tool."""
    require_env("OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL")
    with tempfile.TemporaryDirectory(prefix="mcp-live-all-tools-") as raw:
        cwd = Path(raw)
        (cwd / "src").mkdir()
        (cwd / "src" / "GLOB_AI_OK.rs").write_text("// rust fixture\n", encoding="utf-8")
        (cwd / "read.txt").write_text("READ_AI_OK\n", encoding="utf-8")
        (cwd / "edit.txt").write_text("before AI edit\n", encoding="utf-8")
        (cwd / "patch.txt").write_text("before patch\n", encoding="utf-8")
        (cwd / "grep.txt").write_text("line one\nGREP_AI_OK\n", encoding="utf-8")
        skill = cwd / ".agents" / "skills" / "live-ai-skill"
        skill.mkdir(parents=True)
        skill_text = (
            "---\nname: live-ai-skill\n"
            "description: Gives the unique instruction SKILL_AI_OK.\n---\n"
            "When asked for the skill marker, answer SKILL_GET_AI_OK.\n"
        )
        (skill / "SKILL.md").write_text(skill_text, encoding="utf-8")

        with ExitStack() as stack:
            servers, routes = exact_servers(stack, cwd)
            tools = namespaced_tool_defs(routes)

            result, _, messages = force_namespaced_call(
                routes, tools, "files_read", 'Read "read.txt" and use its content.'
            )
            if "READ_AI_OK" not in str(structured(result)["stdout"]):
                raise RuntimeError("files_read did not return fixture")
            assert_model_consumes(messages, "READ_AI_OK", "files_read")
            print("PASS live_tool_files_read")

            result, _, messages = force_namespaced_call(
                routes,
                tools,
                "files_write",
                'Create "nested/write.txt" with content exactly "WRITE_AI_OK\\n".',
            )
            if (cwd / "nested" / "write.txt").read_text(encoding="utf-8") != "WRITE_AI_OK\n":
                raise RuntimeError("files_write content mismatch")
            assert_model_consumes(messages, "WRITE_AI_OK", "files_write")
            print("PASS live_tool_files_write")

            result, _, messages = force_namespaced_call(
                routes,
                tools,
                "files_edit",
                'In "edit.txt", replace exactly "before AI edit" with "EDIT_AI_OK".',
            )
            if (cwd / "edit.txt").read_text(encoding="utf-8") != "EDIT_AI_OK\n":
                raise RuntimeError("files_edit content mismatch")
            assert_model_consumes(messages, "EDIT_AI_OK", "files_edit")
            print("PASS live_tool_files_edit")

            diff = "--- patch.txt\n+++ patch.txt\n@@ -1 +1 @@\n-before patch\n+PATCH_AI_OK\n"
            result, patch_arguments, messages = force_namespaced_call(
                routes,
                tools,
                "files_patch",
                f'Apply this unified diff to "patch.txt":\n{diff}',
            )
            if (cwd / "patch.txt").read_text(encoding="utf-8").rstrip("\n") != "PATCH_AI_OK":
                raise RuntimeError(f"files_patch content mismatch for arguments {patch_arguments!r}")
            assert_model_consumes(messages, "PATCH_AI_OK", "files_patch")
            print("PASS live_tool_files_patch")

            result, _, messages = force_namespaced_call(
                routes, tools, "search_glob", 'Find Rust files matching exactly "./src/*.rs".'
            )
            if "GLOB_AI_OK.rs" not in str(structured(result)["stdout"]):
                raise RuntimeError("search_glob did not find fixture")
            assert_model_consumes(messages, "GLOB_AI_OK.rs", "search_glob")
            print("PASS live_tool_search_glob")

            result, _, messages = force_namespaced_call(
                routes, tools, "search_grep", "Search for the exact marker GREP_AI_OK."
            )
            grep_output = str(structured(result)["stdout"])
            if "./grep.txt:2:GREP_AI_OK" not in grep_output:
                raise RuntimeError("search_grep path/line/text mismatch")
            assert_model_consumes(messages, "GREP_AI_OK", "search_grep")
            print("PASS live_tool_search_grep")

            result, _, messages = force_namespaced_call(
                routes, tools, "web_fetch", "Fetch https://example.com and inspect the real page."
            )
            if "Example Domain" not in str(structured(result)["stdout"]):
                raise RuntimeError("web_fetch did not return example.com")
            assert_model_consumes(messages, "Example Domain", "web_fetch")
            print("PASS live_tool_web_fetch")

            query = os.environ.get("LIVE_SEARCH_QUERY", "Model Context Protocol")
            result, _, messages = force_namespaced_call(
                routes, tools, "web_search", f"Search the real web for exactly: {query}"
            )
            search_body = str(structured(result)["stdout"])
            if "duckduckgo" not in search_body.lower() or "result" not in search_body.lower():
                raise RuntimeError("web_search did not return plausible DuckDuckGo Lite HTML")
            assert_model_consumes(messages, "DuckDuckGo", "web_search")
            print("PASS live_tool_web_search")

            result, _, messages = force_namespaced_call(
                routes,
                tools,
                "shell_run",
                "Run exactly: printf 'SHELL_AI_OK\\n' && uname -s",
            )
            shell_output = str(structured(result)["stdout"])
            if "SHELL_AI_OK" not in shell_output or "Darwin" not in shell_output:
                raise RuntimeError("shell_run marker/platform mismatch")
            assert_model_consumes(messages, "SHELL_AI_OK", "shell_run")
            print("PASS live_tool_shell_run")

            # The model starts the exact task tool; the harness then observes and polls
            # the negotiated MCP Tasks lifecycle before returning the real result.
            public_name = "tasks_run"
            messages = [{"role": "user", "content": "Start a 1500 ms task with message TASK_AI_OK."}]
            response = openai_chat(
                messages,
                tools=[tool for tool in tools if tool["function"]["name"] == public_name],
                tool_choice={"type": "function", "function": {"name": public_name}},
            )
            assistant = assistant_message(response)
            calls = assistant.get("tool_calls")
            if not isinstance(calls, list) or len(calls) != 1:
                raise RuntimeError("tasks_run model call missing")
            call = calls[0]
            if call.get("function", {}).get("name") != public_name:
                raise RuntimeError("model selected the wrong task tool")
            raw_arguments = call["function"].get("arguments", "{}")
            arguments = raw_arguments if isinstance(raw_arguments, dict) else json.loads(raw_arguments)
            started_at = time.monotonic()
            started = servers["tasks"].request(
                "tools/call", {"name": "run", "arguments": arguments}, tasks=True
            )["result"]
            if started.get("resultType") != "task":
                raise RuntimeError("tasks_run did not return a task handle")
            servers["tasks"].acknowledge_task(started)
            if servers["tasks"].task_state(started)["status"] != "working":
                raise RuntimeError("tasks_run had no observable intermediate state")
            task_result = servers["tasks"].wait_task(started)
            if time.monotonic() - started_at < 1.0 or "TASK_AI_OK" not in str(structured(task_result)["stdout"]):
                raise RuntimeError("tasks_run was not genuinely delayed or returned wrong output")
            task_text = task_result.get("content", [{}])[0].get("text", "")
            messages.extend(
                [
                    {key: assistant[key] for key in ("role", "content", "tool_calls") if key in assistant},
                    {"role": "tool", "tool_call_id": call["id"], "content": task_text},
                ]
            )
            assert_model_consumes(messages, "TASK_AI_OK", "tasks_run")
            print("PASS live_tool_tasks_run")

            result, _, messages = force_namespaced_call(
                routes, tools, "skills_list", "Discover the available local Agent Skills."
            )
            if "live-ai-skill" not in str(structured(result)["stdout"]):
                raise RuntimeError("skills_list did not return fixture")
            assert_model_consumes(messages, "live-ai-skill", "skills_list")
            print("PASS live_tool_skills_list")

            result, arguments, messages = force_namespaced_call(
                routes,
                tools,
                "skills_get",
                "Load the exact skill_id live-ai-skill and inspect its complete instructions.",
            )
            if arguments.get("skill_id") != "live-ai-skill" or skill_text not in str(structured(result)["stdout"]):
                raise RuntimeError("skills_get did not load the exact listed skill")
            assert_model_consumes(messages, "SKILL_GET_AI_OK", "skills_get")
            print("PASS live_tool_skills_get")


def test_live_media_workflow() -> None:
    """Use real model-selected MCP arguments and actual files, not mocked calls."""
    require_env("OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL")
    if not (ROOT.parent / "cookies.txt").is_file():
        raise RuntimeError("place cookies.txt in the workspace root")
    with tempfile.TemporaryDirectory(prefix="mcp-standalone-media-") as raw:
        cwd = Path(raw)
        shutil.copyfile(ROOT / "mcp.media.workflow.json", cwd / "mcp.media.workflow.json")
        (cwd / "cookies.txt").symlink_to(ROOT.parent / "cookies.txt")
        config = json.loads((cwd / "mcp.media.workflow.json").read_text())
        test_media_server(config, cwd)


def test_media_server(config: dict[str, object], cwd: Path) -> None:
    """Only the copied workflow JSON and cookies exist: no helper script."""
    import hashlib

    with McpProcess(config["servers"]["media"]["args"], cwd) as mcp:
        names = ("fetch_funny", "generate_image", "edit_image", "compare_images")
        advertised = {item["name"]: item for item in mcp.tools()}
        if set(advertised) != set(names):
            raise RuntimeError(f"unexpected media tools: {list(advertised)}")
        tools = [{"type": "function", "function": {
            "name": name, "description": advertised[name]["description"],
            "parameters": advertised[name]["inputSchema"]}} for name in names]
        messages: list[dict[str, object]] = []

        denied = mcp.call("edit_image", {"image": str(cwd / "cookies.txt"), "prompt": "read it"})
        if not denied.get("isError"):
            raise RuntimeError("filesystem policy allowed the private cookie path")
        invalid = structured(mcp.call("fetch_funny", {"sort": "new", "posts": 0}))
        if invalid["exitCode"] == 0:
            raise RuntimeError("unbounded Reddit pagination was accepted")

        def step(name: str, prompt: str) -> dict[str, object]:
            result, _ = forced_step(messages, {name: mcp}, name, prompt, tools)
            values = structured(result)
            if values["exitCode"] != 0 or values["timedOut"] or values["stdoutTruncated"]:
                raise RuntimeError(f"{name} failed: {str(values['stderr'])[:400]}")
            arguments = json.loads(messages[-2]["tool_calls"][0]["function"]["arguments"])
            print(f"PASS {name} model parameters: {json.dumps(arguments, ensure_ascii=False)[:180]}")
            return json.loads(values["stdout"])

        fetched = step("fetch_funny", "Download the first 5 new posts from the funny subreddit. Choose the tool parameters yourself.")
        if fetched["sort"] != "new" or len(fetched["posts"]) != 5:
            raise RuntimeError("the model did not retrieve five new Reddit posts")
        before = next((p["image"] for p in fetched["posts"] if p["image"]), None)
        if before is None or not Path(before).is_file():
            raise RuntimeError("Reddit returned no downloaded post image")

        modified = step("edit_image", f"Use the image at {before} from the Reddit result. Add a vivid purple border and a large yellow star in the upper-left corner. Keep the original scene recognizable.")
        after = modified["edited"]
        if modified["original"] != before or not Path(after).is_file():
            raise RuntimeError("image tool did not edit the selected Reddit image")
        if hashlib.sha256(Path(before).read_bytes()).digest() == hashlib.sha256(Path(after).read_bytes()).digest():
            raise RuntimeError("edited image bytes are identical to the original")

        comparison = step("compare_images", f"Compare original image {before} and edited image {after}. Describe the visible differences, especially any new border or star.")
        description = comparison["description"]
        if len(description) < 30 or not all(word in description.lower() for word in ("border", "star")):
            raise RuntimeError(f"comparison model did not describe the differences: {description!r}")
        print(f"PASS live media workflow: {description[:240]}")

        created = step("generate_image", "Generate a new image of a serene mountain landscape at sunset with dramatic clouds.")
        if created["original"] is not None or not Path(created["edited"]).is_file():
            raise RuntimeError("image generation did not save a new image")


def main() -> int:
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--llm", action="store_true", help="run single-model OpenRouter round trips (.env OPENAI_MODEL)")
    group.add_argument("--examples", action="store_true", help="run every mcp.example.json tool")
    group.add_argument("--models", action="store_true", help="run the multi-step scenario suite across every LIVE_MODELS model")
    group.add_argument(
        "--live-tools",
        action="store_true",
        help="run real public web checks and real-model calls for all exact example tools",
    )
    group.add_argument("--all", action="store_true", help="run the --llm and --examples suites")
    group.add_argument("--media", action="store_true", help="run live Reddit → Muse Image → DeepSeek MCP workflow")
    args = parser.parse_args()

    load_dotenv(ROOT.parent / ".env")
    load_dotenv(ROOT / ".env")
    selected_llm = args.llm or args.all or not (args.examples or args.models or args.live_tools or args.media)
    selected_examples = args.examples or args.all
    selected_models_mode = args.models

    if selected_llm:
        test_openai_connection()
        test_llm_shell()
        test_llm_durable_task()
    if selected_examples:
        test_examples()
    if selected_models_mode:
        return run_multi_model_suite()
    if args.live_tools:
        test_public_web_examples()
        test_live_example_tools()
    if args.media:
        test_live_media_workflow()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"FAIL {error}", file=sys.stderr)
        raise SystemExit(1)
