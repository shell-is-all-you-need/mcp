<p align="center">
  <img src="./logo.svg" alt="shell-is-all-you-need" width="900">
</p>

# shell-is-all-you-need

A dependency-free Rust [Model Context Protocol](https://modelcontextprotocol.io/) server that turns fixed process invocations into small, explicit tools for AI clients.

Use it to give an assistant focused capabilities—reading selected files, searching a workspace, running a formatter, or invoking an internal CLI—without writing a custom MCP server or giving every tool unrestricted shell access.

## Why use it?

- **One binary, no runtime dependencies.** The MCP server is implemented with the Rust standard library.
- **Multiple tools per server.** Each tool has its own name, schema, command, filesystem policy, rate limit, timeout, output limit, and concurrency limit.
- **Direct process execution.** Commands are fixed argv arrays; a shell is used only when you explicitly configure one.
- **Structured results.** Calls return exit code, stdout, stderr, timeout state, and output-truncation state.
- **Filesystem boundaries.** Path inputs can be restricted to allowed roots and denied subtrees, including protection against symlink escapes.
- **Optional durable tasks.** Long-running tools can use MCP Tasks with persisted state, polling, cancellation, and recovery.

## Install

### Homebrew

```sh
brew install shell-is-all-you-need/tap/shell-is-all-you-need
```

Or add the tap once and use the short name:

```sh
brew tap shell-is-all-you-need/tap
brew install shell-is-all-you-need
```

### Rust

```sh
cargo install shell-is-all-you-need
```

### npm

Node.js 24 or newer is required.

```sh
npm install -g shell-is-all-you-need
```

Or run it without a global installation:

```sh
npx -y shell-is-all-you-need --help
```

### Python

PyPI uses only the canonical `shell-is-all-you-need` project and command.

```sh
pipx install shell-is-all-you-need
# or
pip install shell-is-all-you-need
```

You can also run it temporarily:

```sh
uvx shell-is-all-you-need --help
pipx run shell-is-all-you-need --help
```

The npm and Python launchers download the matching versioned binary from GitHub Releases on first use, verify its SHA-256 checksum, and cache it locally. They support Linux, macOS, and Windows on x86-64 and ARM64. Set `SHELL_IS_ALL_YOU_NEED_BINARY=/absolute/path/to/shell-is-all-you-need` to use an existing binary instead.

## Quick start

This server exposes a `run` tool that invokes `rustc --version`:

```sh
shell-is-all-you-need \
  --tool \
  --name run \
  --description "Print the installed Rust compiler version." \
  --input-schema '{"type":"object","properties":{},"additionalProperties":false}' \
  --exec rustc --version
```

The process communicates over MCP on standard input and output, so it is normally started by an AI client rather than run interactively.

## Configure an AI client

Add the command and arguments to your client's MCP server configuration. Most clients use a shape similar to this:

```json
{
  "mcpServers": {
    "rust": {
      "command": "shell-is-all-you-need",
      "args": [
        "--tool",
        "--name",
        "run",
        "--description",
        "Print the installed Rust compiler version.",
        "--input-schema",
        "{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false}",
        "--exec",
        "rustc",
        "--version"
      ]
    }
  }
}
```

Some clients call the top-level setting `servers` or place MCP configuration in a larger application-specific object. Keep the server name (`rust` above), command, and argument list unchanged when adapting it.

If a desktop application cannot find the installed command, use its absolute path. Find it with:

```sh
command -v shell-is-all-you-need
```

Restart or reload the client after changing its MCP configuration. The client should discover a model-facing tool named `rust_run`.

## Ready-to-use tool collection

[`mcp.example.json`](./mcp.example.json) contains a complete configuration for these tools:

| Server | Model-facing tools |
| --- | --- |
| `files` | `files_read`, `files_write`, `files_edit`, `files_patch` |
| `search` | `search_glob`, `search_grep` |
| `web` | `web_fetch`, `web_search` |
| `shell` | `shell_run` |
| `tasks` | `tasks_run` |
| `skills` | `skills_list`, `skills_get` |

Copy its entries into the MCP section used by your client. The examples assume a Unix-like environment with `sh`, `bash`, `cat`, `find`, `grep`, `patch`, and `python3` available.

The collection is intended as a practical starting point:

- File tools are restricted to the working directory and deny `.env`, `.git`, and `.mcp-tasks`.
- `search_grep` excludes those sensitive locations.
- `web_fetch` accepts only HTTP and HTTPS URLs and uses Python `urllib`.
- `web_search` uses Python `urllib` with DuckDuckGo Lite at `https://lite.duckduckgo.com/lite/?q={query}&kl=en-us&kp=0`; neither web tool depends on `curl` or `wget`.
- `tasks_run` is an inline long-running mock for trying the MCP Tasks lifecycle.
- Skill tools discover and load Agent Skills under `.agents/skills`.

Run the client with the workspace you want to expose as its working directory. Relative filesystem roots and commands in the example are resolved there.

## Define your own tools

Each `--tool` begins an independent definition. `--exec` starts the fixed child-process argv, and the next exact `--tool` begins another definition.

This example exposes separately constrained read and compiler-version tools:

```sh
shell-is-all-you-need \
  --tool \
  --name read \
  --description "Read a UTF-8 file in the workspace." \
  --input-schema '{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}' \
  --fs-path-field path \
  --fs-root . \
  --fs-deny-path .env \
  --fs-deny-path .git \
  --exec cat -- '{path}' \
  --tool \
  --name version \
  --description "Print the Rust compiler version." \
  --input-schema '{"type":"object","properties":{},"additionalProperties":false}' \
  --exec rustc --version
```

Input schemas use a deliberately small, closed, flat subset of JSON Schema 2020-12. Fields may be `string`, `number`, `integer`, or `boolean`. Every declared field must be required and used by the command templates, and undeclared properties are rejected.

Use `{field}` in an argv item to substitute a validated input value. Double opening or closing braces when the command needs a literal brace.

Run this for the complete option reference:

```sh
shell-is-all-you-need --help
```

## Tool naming

Use a stable server namespace and short verb-like tool names. AI clients commonly expose MCP tools as `<server>_<tool>`:

- server `files` + tool `read` → `files_read`
- server `shell` + tool `run` → `shell_run`

This keeps related tools grouped and avoids duplicated names such as `shell_shell`.

## Safety

`shell-is-all-you-need` enforces the boundaries you configure, but it cannot make an inherently broad command safe. Prefer narrow, fixed executables and arguments over a general `sh -c` tool.

For path arguments:

1. Declare each path field with `--fs-path-field`.
2. Restrict it with one or more `--fs-root` values.
3. Deny secrets, metadata, and task storage with `--fs-deny-path`.
4. Give write tools narrower roots than read tools when possible.

Allowed roots and denied paths are checked after path and symlink resolution, and denied paths take precedence. Configure process timeouts, output limits, concurrency limits, and invocation rate limits for commands that may be expensive or untrusted.

Only expose a raw shell tool such as `shell_run` when the AI client and workspace are trusted and the broader access is intentional.

## MCP Tasks

Add `--task-store-dir DIR` to make a tool task-capable. Task records are persisted, recover after server restarts, and can use a retention TTL. Use a separate canonical task-store directory for each task-enabled tool.

The server advertises MCP protocol version `2026-07-28`, structured tool output, and the MCP Tasks extension when task support is configured and negotiated by the client.

## License

MIT
