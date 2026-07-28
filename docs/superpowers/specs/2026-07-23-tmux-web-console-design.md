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
## 单 MCP 实例并发保护（2026-07-28，临时边界）

- 同一个 MCP 进程内，同一 socket/pane 同时只允许一个 tracked command；后续
  `execute-command` 直接拒绝，不排队。不同 pane 仍可并行。
- AI 向运行中的 tracked command 发送 `send-keys`、`send-hex`、`paste-text`
  或特殊键时必须提供匹配的 `forCommandId`；`send-cancel`（Ctrl-C）例外，但
  不会因此提前释放命令占用。
- `rawMode` 与 `noEnter` 不出现在 MCP 工具 schema 中；AI 的
  `execute-command` 固定使用 tracked 模式并发送 Enter。网页交互仍走独立按键接口。
- `TrackingError` 表示 pane 状态不确定并继续占用该 pane。下一次 AI 工具调用先
  自动执行 `capture-pane`：成功时返回画面、取消本次调用并解除不确定；失败时设置
  “AI 操作已暂停”，之后拒绝 AI 工具调用，直到人在网页清除暂停。
- 网页接入时暂停标记保存在本机状态目录的 `ai-paused.json`；未接入网页时只保存在
  当前 MCP 进程内，重启进程即可清除。
- **临时边界：** 当前不协调多个 Codex/Claude/MCP 进程，不把普通命令占用写入远端
  tmux 元数据，也不锁定 pane 的删除、移动或合并操作。多个 MCP 同时操作同一 pane
  仍可能冲突；只有出现实际需求时再增加跨进程协调。

## AI 请求空闲保护（仅记录，暂不实现）

- 独立模块、独立开关，不耦合 Gate、命令跟踪或网页状态。
- 默认开启，超时为 60 分钟；配置可调整或关闭，暂不提供网页选项。
- 仅统计 AI 主动发起的 `tools/call`；协议初始化、工具列表、资源订阅和后台轮询不计入。
- 连续超时后，下一次 `tools/call` 直接拒绝，要求 AI 先探查远程主机和 tmux 状态；不自动执行或重放原请求。
- 探查完成后的下一次主动工具请求重新开始计时。
