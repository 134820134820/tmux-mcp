# tmux MCP：一个 MCP 连接多个服务器账号

每个 agent 客户端只注册一个 `tmux` MCP。每次调用使用 `target` 指定服务器和账号；例如 `milab-seven-admin` 和 `milab-seven-intern` 是两个独立 SSH 登录目标。

本文启动命令使用仓库根目录的 `tmux-mcp.exe`。2026-09-22 已更新为 v0.6.1，包含日志埋点、目标隔离、等待修复、异常窗口保护和只读快照；具体边界见 [改进记录](docs/IMPROVEMENTS.md)。已有客户端须重新连接 MCP 才能加载新版。

## SSH 与目标列表

本地 OpenSSH 配置负责主机、端口、账号和私钥，先确保下面这样的命令可以免密码登录：

```powershell
ssh milab-seven-intern
```

MCP 使用独立的 [targets.toml](targets.toml) 清单，只列出需要向 agent 提供的 SSH 别名，并不会自动开放本地所有 SSH 配置：

```toml
[targets.milab-seven-admin]
note = "7号机管理员账号，仅在需要 sudo 时使用"

[targets.milab-seven-intern]
note = "7号机日常训练和普通任务账号"
```

当前清单：

| target | SSH 用户 | 用途 |
|---|---|---|
| milab-seven-admin | hjk | 7 号机管理员，需 sudo 时使用 |
| milab-seven-intern | hjk_intern | 7 号机日常训练、普通任务 |
| milab-eight-admin | hjk | 8 号机管理员 |
| milab-eight-intern | hjk_intern | 8 号机普通任务 |
| milab-ten-admin | hjk | 10 号机管理员；目前只配置这一个账号 |

添加目标时，先配置并验证同名 SSH alias，再将它加入清单，重启 MCP 连接以刷新客户端发现信息。无需再增加一个 MCP 注册。

## Codex

在 `%USERPROFILE%\.codex\config.toml` 中保留一个注册；替换现有 `tmux` 条目，不要重复添加：

```toml
[mcp_servers.tmux]
command = 'E:\buyi_work\tmux-mcp\tmux-mcp.exe'
args = ["--targets", "E:\\buyi_work\\tmux-mcp\\targets.toml", "--web-url", "http://127.0.0.1:38473", "--client-name", "Codex"]
```

按实际仓库位置替换两个绝对路径，重启 Codex 使配置生效。

## Claude Code

在仓库根目录运行以下 PowerShell 命令：

```powershell
$exe = (Resolve-Path .\tmux-mcp.exe).Path
$targets = (Resolve-Path .\targets.toml).Path
claude mcp add --scope user --transport stdio tmux -- $exe --targets $targets --web-url http://127.0.0.1:38473 --client-name "Claude Code" --claude-channel
```

从旧配置迁移时，只移除已存在的旧注册，再添加统一的 `tmux`：

```powershell
claude mcp remove --scope user tmux-8
claude mcp remove --scope user tmux-10
```

若 `tmux` 已存在，应先更新或移除该条目再添加。本机已完成迁移，无需重复运行。

`user` 对当前用户所有项目生效；`local` 和 `project` 配置与项目目录有关，检查/移除时应进入原项目目录，并使用对应 scope：

```powershell
claude mcp list
claude mcp get tmux
```

重启 Claude Code 使配置生效。需要 GPU Channel 回调时，使用：

```powershell
claude --dangerously-load-development-channels server:tmux
```

## 调用与首次使用

所有目标操作都要携带 `target`，例如 `get-tmux-state` 的参数：

```json
{"target": "milab-seven-intern"}
```

新账号没有 tmux 会话时，SSH 仍可能已连接成功。使用 `create-session` 创建一个任务会话，再使用返回的 session/pane ID：

```json
{"target": "milab-seven-intern", "name": "agent-work"}
```

删除会话会终止其中的程序。已有任务应使用其原 pane；测试应使用另外命名的 session 或独立 socket。

**v0.6.1：** 根目录程序默认返回 21 个工具，包含 `list-targets`、`file-stat`、`gpu-snapshot`，目标操作的 schema 要求填写 `target`。

**2026-10-02 版本：** 默认 22 个工具（启用 Claude Channel 后 25 个）。

- **常驻 SSH 连接：** 每个 target 复用已建立的 SSH 连接，同一台机器上第二次起的读取和截屏从 4–11 秒降到 1 秒以内。远端需要 bash、base64、mktemp（Ubuntu 默认具备），不满足时自动回退为每次新建连接。设置 `TMUX_MCP_SSH_POOL=0` 可关闭。
- **`write-file`：** 原子写入单个文本文件，默认不覆盖；`overwrite` 时只要目录可写就替换，并保留原权限；拒绝写穿符号链接。网络在写入完成前后断开时报告"结果不确定"，提示先检查再重试。它属于修改操作，受 Gate 审批约束。
- **`execute-command` 的 `script`：** 多行 bash 脚本先上传到 `~/.cache/tmux-mcp/scripts/` 并用 `bash -n` 检查语法，再作为一条受追踪的 `bash <文件>` 执行。脚本在子 shell 中运行，`cd`/`export` 不会保留到 pane。脚本文件保留 7 天。
- **`notify: true`：** 需要 `--claude-channel`，命令结束时通过 Channel 通知，替代反复轮询 `get-command-result`。
- **网络中断不再暂停 AI：** 等待命令完成的 SSH 连接断开后，改为退避轮询远端退出码（输出很长、开始标记已滚出屏幕时同样能结束），只有 pane 或 tmux 服务确实消失时才报告状态不确定。
- **未提交输入保护：** `paste-text`、不带回车的 `send-keys` 或方向键/Tab 等在空闲 pane 留下输入后，该 pane 的 `execute-command` 和下一次 `paste-text` 会被拒绝，直到按回车（`send-enter`）执行或 Ctrl-C（`send-cancel`）丢弃，避免两段内容拼成一条命令。只记录本 MCP 进程发出的输入。
- **资源列表：** 只列出能读取的带 target 地址（每个 target 的 server/info、clients 和已追踪命令）；pane、window、session 通过 `tmux://{target}/…` 模板或 `get-tmux-state` 获取，列表时不再逐台连接。

Windows 上自动使用系统自带的 OpenSSH（`C:\Windows\System32\OpenSSH\ssh.exe`）；Git 自带的 `ssh` 在这里会卡到超时，即使它在 PATH 中排在前面也不会被使用。需要指定其他 ssh 时设置 `TMUX_MCP_SSH_PROGRAM`。

一个任务复用自己已确认可用的 pane；不接管陌生窗口或在残留输入后追加命令。发送失败、执行状态不确定时，停止修改并向用户报告，不自动清理、打断或换窗口重跑。只读查看不会解除保护。`detach` 默认关闭；开启仅取消完成追踪，仍保留窗口占用保护。

长任务应事先安排程序日志，再使用 `read-file` 分段查看。MCP 不保存完整任务日志，也不会为补齐截断输出而重跑命令。写脚本或配置文件用 `write-file`，多行命令用 `execute-command` 的 `script`，不要用 `paste-text` 粘贴 heredoc。

默认只暴露核心工具；需要 buffer、布局、重命名、删除等高级工具时，可增加 `--full-tools` 并重新连接。工具分类见 [docs/TOOL_SURFACE.md](docs/TOOL_SURFACE.md)。

## 可选 Web 控制中心

Web 控制中心使用 `targets.toml` 中的目标列表，并在页面顶部切换服务器/账号。`--ssh` 可选：传入时指定首次打开的默认 SSH alias；省略时默认使用清单中的第一个目标。

```powershell
$targets = (Resolve-Path .\targets.toml).Path
.\tmux-mcp.exe --web --web-bind 127.0.0.1:38473 --targets $targets
```

如果希望指定默认目标，再追加 `--ssh milab-ten-admin`；页面打开后仍可切换到清单中的其他目标。

打开 `http://127.0.0.1:38473/`，在“目标”下拉框中切换 `milab-seven-admin`、`milab-seven-intern`、`milab-eight-admin`、`milab-eight-intern` 或 `milab-ten-admin`。客户端的 `--web-url` 连接到该服务，不会替你启动它。控制中心状态保存在 `%LOCALAPPDATA%\tmux-mcp`，其中包括调用日志、Gate 状态和 token；不要提交该目录。

不使用 Web 时，从客户端参数中移除 `--web-url` 和 `--client-name`，保留 `--targets`：

```powershell
.\tmux-mcp.exe --targets E:\buyi_work\tmux-mcp\targets.toml
```

## 构建与测试版本

当前注册指向仓库根目录的 `tmux-mcp.exe`。源码与已使用程序可能处于不同阶段，不应直接重建覆盖运行文件。

开发验证使用独立构建目录：

```powershell
cargo build --release --target-dir target/test-build
```

产物为 `target/test-build/release/tmux-mcp-rs.exe`，可另命名为 `tmux-mcp-test.exe`。测试实例使用单独的配置、socket 和状态目录，验证完成前不替换客户端注册，也不操作工作会话。

正式部署脚本 `scripts/build-release.ps1` 会覆盖根目录 `tmux-mcp.exe`；仅在准备正式切换版本时执行。
