# tmux MCP 启动指南

以下命令中的 `tmux-mcp-rs.exe` 可替换为 exe 的绝对路径，`user@host` 替换为 SSH 地址。

## 启动网页控制中心

```powershell
tmux-mcp-rs.exe --web --web-bind 127.0.0.1:38473 --ssh user@host
```

打开：

```text
http://127.0.0.1:38473/
```

## Codex

在 `%USERPROFILE%\.codex\config.toml` 中添加：

```toml
[mcp_servers.tmux]
command = 'C:\path\to\tmux-mcp-rs.exe'
args = ["--ssh", "user@host", "--web-url", "http://127.0.0.1:38473", "--client-name", "Codex"]
```

重启 Codex。

## Claude Code

```powershell
claude mcp add --transport stdio tmux -- tmux-mcp-rs.exe --ssh user@host --web-url http://127.0.0.1:38473 --client-name "Claude Code"
```

## 不使用网页

Codex：

```toml
[mcp_servers.tmux]
command = 'C:\path\to\tmux-mcp-rs.exe'
args = ["--ssh", "user@host"]
```

Claude Code：

```powershell
claude mcp add --transport stdio tmux -- tmux-mcp-rs.exe --ssh user@host
```
