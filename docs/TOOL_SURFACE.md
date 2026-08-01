# MCP 工具暴露

## 默认核心工具

未加 `--full-tools` 时，stdio MCP 只暴露 `@agent-core`：

- tmux 状态：`get-tmux-state`、`capture-pane`
- 文件只读：`list-directory`、`read-file`、`find-files`、`search-text`
- Git 只读：`git-status`、`git-diff`、`git-log`、`git-show`
- 命令：`execute-command`、`get-command-result`
- 创建：`create-session`、`create-window`、`split-pane`
- 输入：`send-keys`、`paste-text`、`press-special-key`
- GPU（仅加 `--claude-channel`）：`watch-gpu-idle`、`get-gpu-watch`、`stop-gpu-watch`

默认共 18 个工具；启用 Claude Channel 后为 21 个。

## 默认隐藏工具

以下实现保留，但默认不进入 MCP `tools/list`：

- socket/客户端：`socket-for-path`、`list-clients`、`detach-client`
- paste buffer：`list-buffers`、`show-buffer`、`save-buffer`、`load-buffer`、`delete-buffer`、`set-buffer`、`append-buffer`、`rename-buffer`、`search-buffer`、`subsearch-buffer`
- 管理：`kill-target`、`rename-target`
- 导航/布局：`move-window`、`select-window`、`select-pane`、`resize-pane`、`zoom-pane`、`select-layout`、`join-pane`、`break-pane`、`swap-pane`、`set-synchronize-panes`
- 低层输入：`send-hex`

网页控制中心直接调用内部 tmux 操作，不受 MCP 工具隐藏影响。

## 加载隐藏工具

隐藏工具不能由 AI 通过提示词自行加载。修改 MCP 启动参数后重启对应客户端。

加载全部工具：

```powershell
.\tmux-mcp.exe --ssh milab-ten --full-tools
```

Codex/Claude Code 配置中同样只需在现有 `args` 末尾增加：

```text
--full-tools
```

只加载部分高级分组时，同时使用 `--full-tools` 和现有工具过滤器。例如只开放核心工具和布局工具：

```powershell
$env:TMUX_MCP_TOOLS = "allow:@agent-core,@move"
.\tmux-mcp.exe --ssh milab-ten --full-tools
```

也可写入 `config.toml`：

```toml
[security.tools]
mode = "allow"
items = ["@agent-core", "@move"]
```

可用分组包括 `@agent-core`、`@read`、`@file-read`、`@git-read`、`@buffer-read`、`@buffer-write`、`@list`、`@capture`、`@create`、`@split`、`@kill`、`@execute`、`@gpu-monitor`、`@rename`、`@move`、`@interactive`、`@special-keys`、`@raw-input`、`@socket` 和 `@all`。

`TMUX_MCP_TOOLS`/`[security.tools]` 只能进一步缩小 `--full-tools` 的工具面，不能绕过其他安全策略。

## 已合并的旧工具

以下旧名称不再暴露：

- `list-sessions`、`find-session`、`list-windows`、`list-panes`、`get-current-session` → `get-tmux-state`
- `kill-session`、`kill-window`、`kill-pane` → `kill-target`，通过 `$N`、`@N`、`%N` 识别目标类型
- `rename-session`、`rename-window`、`rename-pane` → `rename-target`
- `send-cancel`、`send-eof`、`send-escape`、`send-enter`、`send-tab`、`send-backspace`、`send-up`、`send-down`、`send-left`、`send-right`、`send-page-up`、`send-page-down`、`send-home`、`send-end` → `press-special-key`
