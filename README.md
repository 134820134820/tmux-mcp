# tmux MCP 启动指南

## 构建

```powershell
& "D:\DaveWorks\tmux-mcp\scripts\build-release.ps1"
```

可直接使用和提交的产物：

```text
D:\DaveWorks\tmux-mcp\tmux-mcp.exe
```

## 控制中心

```powershell
& "D:\DaveWorks\tmux-mcp\tmux-mcp.exe" --web --web-bind 127.0.0.1:38473 --ssh milab-ten
```

打开 `http://127.0.0.1:38473/`。

## Codex

在 `%USERPROFILE%\.codex\config.toml` 中添加：

```toml
[mcp_servers.tmux]
command = 'D:\DaveWorks\tmux-mcp\tmux-mcp.exe'
args = ["--ssh", "milab-ten", "--web-url", "http://127.0.0.1:38473", "--client-name", "Codex"]
```

重启 Codex。

## Claude Code

Claude 只记录 exe 路径，不会复制 exe。未加入 `PATH` 时必须使用绝对路径：

```powershell
claude mcp add --scope user --transport stdio tmux -- "D:\DaveWorks\tmux-mcp\tmux-mcp.exe" --ssh milab-ten --web-url http://127.0.0.1:38473 --client-name "Claude Code"
```

`--scope`：

- `local`：默认值，仅当前项目、仅当前用户可用。
- `project`：当前项目共享配置，写入项目 `.mcp.json`。
- `user`：当前 Windows 用户的所有项目可用。

## 不使用控制中心

删除 `--web-url` 和 `--client-name` 即可：

```powershell
& "D:\DaveWorks\tmux-mcp\tmux-mcp.exe" --ssh milab-ten
```
