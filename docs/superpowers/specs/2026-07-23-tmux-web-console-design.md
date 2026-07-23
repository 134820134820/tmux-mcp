# tmux MCP Local Web Console Design

## Purpose

Provide one local page for observing clean MCP command requests/results,
interacting with exact tmux panes, and optionally approving mutating MCP tools.
The existing stdio MCP, SSH adapter, tmux functions, security policy, and
`CommandTracker` remain authoritative.

## Architecture

The binary gains a `--web` mode. It runs a loopback-only HTTP service and uses
the same tmux configuration as the stdio server. Independent stdio MCP
processes opt in with `--web-url` and label themselves with `--client-name`.

`TmuxMcpServer::call_tool` checks the registered tool metadata. A tool with
`readOnlyHint=true` delegates immediately. Every other tool creates an action
record and consults the optional control client before delegating to the
unchanged router.

Gate state is represented by a file in the local state directory. Absence means
off. When off, logging is best-effort and a web outage never blocks MCP. When
on, authorization waits without an application timeout; rejection or an
unreachable web service prevents the tmux mutation.

Tracked commands keep using `CommandTracker`. Terminal lifecycle events update
the original web record with `CommandSnapshot`, which already contains the
clean output, exit code, elapsed time, and truncation flag.

## Web Console

The page groups tmux panes by session/window and keeps one selected pane. Other
pane activity only adds an indicator. Gate requests are global and never change
the selection.

Message mode shows complete tracked commands, AI input tools, and grouped human
input. Other mutations appear in an operation log. Interactive mode polls
`capture-pane` every 250 ms and forwards printable characters and supported
special keys immediately. Human input bypasses Gate but uses the same security
policy and exact pane checks.

This version intentionally uses pane snapshots and key forwarding. It does not
add a PTY, WebSocket, xterm, terminal mouse events, or heuristic removal of text
that already exists in tmux scrollback.

## Persistence and Security

State lives under `%LOCALAPPDATA%\tmux-mcp` on Windows, with XDG/home fallbacks
elsewhere. `events.jsonl` appends whole record snapshots; the last valid line for
an id wins. Corrupt trailing lines are ignored. Compaction at 4 MiB retains 200
messages per pane and 200 global operations.

The service binds only to loopback. A generated token authenticates all API
calls. Mutations additionally validate Host, Origin, JSON content type, body
size, identifiers, and input length. The UI renders untrusted values with
`textContent`.

## Public CLI

- `--web`
- `--web-bind 127.0.0.1:38473`
- `--web-url http://127.0.0.1:38473`
- `--client-name <name>`, default `mcp-<pid>`

No option changes existing behavior unless `--web` or `--web-url` is supplied.

