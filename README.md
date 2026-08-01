# tmux MCP 启动指南

以下 PowerShell 命令均在仓库根目录执行。

## 构建

```powershell
.\scripts\build-release.ps1
```

可直接使用和提交的产物：

```text
.\tmux-mcp.exe
```

## 控制中心

```powershell
.\tmux-mcp.exe --web --web-bind 127.0.0.1:38473 --ssh milab-ten
```

打开 `http://127.0.0.1:38473/`。

stdio MCP 默认只暴露核心工具。需要 buffer、布局、重命名、删除等高级工具时，在现有 MCP 参数末尾增加 `--full-tools` 并重启客户端。完整清单和选择性开放方法见 [docs/TOOL_SURFACE.md](docs/TOOL_SURFACE.md)。

## Codex

在 `%USERPROFILE%\.codex\config.toml` 中添加：

```toml
[mcp_servers.tmux]
command = 'C:\填写仓库的绝对路径\tmux-mcp.exe'
args = ["--ssh", "milab-ten", "--web-url", "http://127.0.0.1:38473", "--client-name", "Codex"]
```

重启 Codex。

## Claude Code

Claude 只记录 exe 路径，不会复制 exe。以下命令自动把当前仓库中的 exe 转成绝对路径：

```powershell
$exe = (Resolve-Path .\tmux-mcp.exe).Path
claude mcp add --scope user --transport stdio tmux -- $exe --ssh milab-ten --web-url http://127.0.0.1:38473 --client-name "Claude Code" --claude-channel
```

启用 GPU 空闲回调时，用开发版 Channel 参数启动 Claude：

```powershell
claude --dangerously-load-development-channels server:tmux
```

随后可让 Claude 调用 `watch-gpu-idle`；用 `get-gpu-watch` 查询，用 `stop-gpu-watch` 停止。未加 `--claude-channel` 时，这三个工具不会出现，原有 MCP 功能不变。

`--scope`：

- `local`：默认值，仅当前项目、仅当前用户可用。
- `project`：当前项目共享配置，写入项目 `.mcp.json`。
- `user`：当前 Windows 用户的所有项目可用。

### 移除

`local` 和 `project` 配置与项目目录有关，先进入当初添加 MCP 的项目目录，再查看名称：

```powershell
claude mcp list
claude mcp get tmux
```

删除命令格式为 `claude mcp remove [--scope local|project|user] <名称>`。例如删除名为 `tmux` 的项目级配置：

```powershell
claude mcp remove --scope project tmux
```

不确定 scope 时可省略 `--scope`，Claude 会删除当前项目上下文中找到的该名称配置：

```powershell
claude mcp remove tmux
```

## 不使用控制中心

删除 `--web-url` 和 `--client-name` 即可：

```powershell
.\tmux-mcp.exe --ssh milab-ten
```
