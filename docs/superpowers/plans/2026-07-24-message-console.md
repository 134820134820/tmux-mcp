# tmux MCP 消息控制台实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:executing-plans`, `superpowers:test-driven-development`,
> `superpowers:systematic-debugging`, and `ponytail:ponytail` to implement this
> plan task-by-task.

**Goal:** 让网页消息模式成为完整命令对话界面：用户先在本地输入整条命令，
提交到明确 pane，并在同一张消息卡中看到运行中状态、干净终态输出、退出码和
耗时；同时把页面改为白色 ChatGPT 风格，保持交互模式现状。

**Architecture:** Web 进程持有一个现有 `CommandTracker` 的 `Arc`。新增命令
API 只做校验、复用现有安全策略并把用户命令交给 tracker；Hub 用
`command_id -> ActionRecord` 的最小映射监听 tracker 事件，将同一条 JSONL
记录从运行中更新到终态。AI MCP 进程、SSH、tmux 包装协议和既有执行语义均不
改变。

**Tech Stack:** Rust 1.70、Tokio、axum 0.7、既有 `CommandTracker`、
原生 HTML/CSS/JavaScript。

## 不可变约束

- 不从 `capture-pane` 屏幕差异猜测命令输出。
- 只有 AI `execute-command` 与消息框提交的网页命令承诺完整终态输出。
- `send-keys`、`paste-text` 和特殊键仍是输入记录，不伪造归属输出。
- 网页人工命令绕过 Gate，但必须通过既有 tool、socket、pane、session 和
  command 安全检查。
- 消息框只接受非空单行命令；不绕过 tracker 的换行限制。
- 网页 tracker 只负责网页命令；不增加跨 MCP 进程调度器。
- 交互模式的 250ms 快照、逐键转发和输入合并逻辑不改。
- 页面继续只用 `textContent` 渲染外部文本，不引入前端框架或新依赖。

---

### Task 1：把现有 CommandTracker 注入 Web 进程

**Files:**

- Modify: `src/main.rs`
- Modify: `src/web.rs`
- Modify: `tests/web.rs`
- Modify: `tests/control_client.rs`
- Modify: `src/server.rs`（仅测试辅助代码）

**Step 1：先写会失败的编译改动**

把 `src/web.rs` 的公开构造函数签名改成：

```rust
pub fn build_router(
    hub: Arc<ControlHub>,
    token: String,
    policy: Arc<SecurityPolicy>,
    socket: Option<String>,
    tracker: Arc<CommandTracker>,
) -> Router
```

并在 `AppContext` 增加：

```rust
tracker: Arc<CommandTracker>,
```

先不要更新调用点，运行：

```powershell
cargo test --test web
```

Expected: 编译失败，所有 `build_router` 调用都提示缺少第 5 个参数。

**Step 2：最小修复生产调用点**

在 `src/main.rs` 的 `--web` 分支创建 tracker：

```rust
let tracker = Arc::new(if cli.config.is_some() {
    CommandTracker::with_tracking(shell_type, tracking_config)
} else {
    CommandTracker::new(shell_type)
});
```

把它作为第 5 个参数传给 `build_router`。stdio MCP 分支继续创建自己的
tracker，不共享、不改现有行为。

**Step 3：最小修复测试调用点**

在 `tests/web.rs` 提供：

```rust
fn test_tracker() -> Arc<CommandTracker> {
    Arc::new(CommandTracker::new(ShellType::Bash))
}
```

所有 router 测试传入新的 tracker。`tests/control_client.rs` 和
`src/server.rs` 内部测试辅助代码同样传入
`Arc::new(CommandTracker::new(ShellType::Bash))`。

**Step 4：验证**

Run:

```powershell
cargo test --test web
```

Expected: 现有 web 测试全部通过，行为未变。

**Step 5：提交**

```powershell
git add src/main.rs src/web.rs src/server.rs tests/web.rs tests/control_client.rs
git commit -m "refactor: share command tracker with web console"
```

---

### Task 2：新增消息模式命令 API，并先锁定拒绝路径

**Files:**

- Modify: `src/web.rs`
- Modify: `tests/web.rs`

**Step 1：写空命令和多行命令的失败测试**

在 `tests/web.rs` 新增
`command_endpoint_rejects_empty_and_multiline_commands`：

- 构造带合法 token、Host、Origin 和 `application/json` 的 router 请求。
- `POST /api/panes/%251/commands`。
- `{"command":"   "}` 期望 `400 BAD_REQUEST`。
- `{"command":"printf one\nprintf two"}` 期望 `400 BAD_REQUEST`。
- 断言 Hub 没有新增消息记录。

Run:

```powershell
cargo test --test web command_endpoint_rejects_empty_and_multiline_commands
```

Expected: 失败，因为路由尚不存在。

**Step 2：写安全策略拒绝测试**

新增 `command_endpoint_applies_existing_command_policy`：

```rust
let mut config = SecurityConfig::default();
config.allow_execute_command = false;
let policy = Arc::new(SecurityPolicy::from_config(config).unwrap());
```

提交合法单行命令并断言：

- 返回 `403 FORBIDDEN`。
- Hub 没有运行中记录。
- tmux/SSH 没有被调用。

Run:

```powershell
cargo test --test web command_endpoint_applies_existing_command_policy
```

Expected: 失败，因为路由尚不存在。

**Step 3：实现最小请求类型、路由和前置校验**

在 `src/web.rs` 增加：

```rust
#[derive(Deserialize)]
struct CommandInput {
    command: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandAccepted {
    command_id: String,
}
```

注册：

```rust
.route("/api/panes/:id/commands", post(send_command))
```

`send_command` 按以下顺序处理，确保无效请求不会触达 tmux：

1. 解码并验证 pane ID。
2. 拒绝 `trim().is_empty()`。
3. 拒绝包含 `\n` 或 `\r`。
4. 调用 `authorize_pane_action(&context, "execute-command", &pane_id)`。
5. 调用 `context.policy.check_command(&input.command)`。

通过后才允许进入 Task 3 的 tracker 执行；本 Task 可暂时返回
`501 NOT_IMPLEMENTED`。

**Step 4：验证拒绝路径**

Run:

```powershell
cargo test --test web command_endpoint_
```

Expected: 两个新测试通过；现有 API 安全测试仍通过。

**Step 5：提交**

```powershell
git add src/web.rs tests/web.rs
git commit -m "feat: validate web command submissions"
```

---

### Task 3：用同一记录追踪网页命令终态

**Files:**

- Modify: `src/web.rs`
- Modify: `tests/web.rs`

**Step 1：写 Hub 记录更新的失败测试**

新增 `web_command_terminal_snapshot_updates_same_record`：

1. 创建来源为“你”、工具为 `execute-command` 的 `ActionRecord`。
2. 调用计划中的 `track_web_command("cmd-1", record)`。
3. 构造一个 terminal `CommandSnapshot`：
   - `command_id = "cmd-1"`
   - `status = Completed`
   - `exit_code = Some(0)`
   - `output = Some("clean output\n")`
   - `truncated = false`
4. 调用 `update_web_command(snapshot)`。
5. 从 Hub state 读取消息，断言：
   - 只有一条记录。
   - record ID 与初始 ID 相同。
   - 状态为 completed。
   - 输出、退出码、耗时和截断字段来自 snapshot。

Run:

```powershell
cargo test --test web web_command_terminal_snapshot_updates_same_record
```

Expected: 编译失败，因为 Hub 尚无这些方法。

**Step 2：实现最小 command 映射**

在 `HubInner` 增加：

```rust
web_commands: Mutex<HashMap<String, ActionRecord>>,
```

在 `ControlHub` 增加：

```rust
fn track_web_command(
    &self,
    command_id: String,
    record: ActionRecord,
) -> io::Result<()>;

fn update_web_command(
    &self,
    snapshot: CommandSnapshot,
) -> io::Result<bool>;

fn forget_web_command(&self, command_id: &str);
```

行为：

- `track_web_command` 先保存 `command_id -> record`，再把 running 记录
  `upsert` 到 JSONL。
- `update_web_command` 只更新映射中的记录，复用
  `ActionRecord::mark_command`，并以相同 record ID `upsert`。
- terminal `Updated` 成功落盘后才移除映射。
- `Evicted` 直接移除映射。

Run:

```powershell
cargo test --test web web_command_terminal_snapshot_updates_same_record
```

Expected: 通过。

**Step 3：写并实现 tracker 事件转发**

在 `build_router` 中只启动一次后台任务：

```rust
let mut events = tracker.subscribe_events();
tokio::spawn(async move {
    while let Ok(event) = events.recv().await {
        // Created: no-op
        // Terminal / terminal Updated: get_command -> CommandSnapshot -> Hub update
        // terminal Updated persisted: forget
        // Evicted: forget
    }
});
```

严格复用 `src/server.rs` 现有终态事件处理顺序：

- `Terminal` 可能只是占位终态，先更新但不提前丢映射。
- terminal `Updated` 才含最终清理输出；写入成功后移除映射。
- 若事件接收滞后，使用 `tracker.get_command(command_id)` 取得最新状态。

**Step 4：实现 API 的成功路径和 race 补偿**

`send_command` 通过前置校验后：

1. 创建 `ActionRecord::new("你", "execute-command", ...)`。
2. `mark_running()`。
3. 调用：

```rust
context
    .tracker
    .execute_command(
        pane_id.clone(),
        input.command.clone(),
        false,
        false,
        None,
        context.socket.clone(),
    )
    .await
```

4. 成功取得 `command_id` 后调用 `track_web_command`。
5. 立即调用一次 `get_command`；若极短命令已终态，马上
   `update_web_command`，补偿“事件先于映射”的竞态。
6. 返回 `202 ACCEPTED` 和 `{"commandId":"..."}`。
7. tracker 启动失败时，把同一 record 标记 failed 并落盘，返回
   `502 BAD_GATEWAY`；不得留下假的 running 记录。

**Step 5：加入真实成功路径的集成测试边界**

不要为测试伪造另一套 tracker trait。已有 `CommandTracker` 单元测试覆盖排队和
终态清理；Web 层用 Hub 更新测试覆盖 record 身份。真实 SSH/tmux 成功路径留给
Task 6 浏览器验收，保持实现最小。

Run:

```powershell
cargo test --test web
```

Expected: 全部 web 测试通过。

**Step 6：提交**

```powershell
git add src/web.rs tests/web.rs
git commit -m "feat: track web commands in message stream"
```

---

### Task 4：消息模式加入本地命令输入框

**Files:**

- Modify: `web/index.html`
- Modify: `tests/web_ui.rs`

**Step 1：写静态契约失败测试**

在 `tests/web_ui.rs` 增加断言：

- 页面包含 `id="command-composer"`。
- 包含 `id="command-input"` 和 `id="command-send"`。
- JavaScript 调用 `/api/panes/${encodeURIComponent(paneId)}/commands`。
- Enter 使用 `requestSubmit()`。
- 仍不包含 `innerHTML`。

Run:

```powershell
cargo test --test web_ui
```

Expected: 失败，因为输入框尚不存在。

**Step 2：添加最小 HTML**

在消息区底部添加：

```html
<form id="command-composer" class="command-composer">
  <label class="sr-only" for="command-input">向当前 pane 发送命令</label>
  <textarea
    id="command-input"
    rows="1"
    maxlength="65536"
    placeholder="向当前 pane 发送命令"
    disabled
  ></textarea>
  <button id="command-send" type="submit" aria-label="发送命令" disabled>
    ↑
  </button>
</form>
```

textarea 只为输入体验和自动高度服务；任何 Enter 都提交，不允许插入换行。

**Step 3：实现提交状态**

新增元素引用和：

```javascript
let commandSubmitting = false;
```

实现 `updateComposer()`：

- 无 `selectedPane` 或提交请求进行中时禁用输入框和发送按钮。
- 有 pane 且非提交中时启用。
- `renderHeading`、pane 选择和模式切换后调用。

表单 submit：

1. 捕获当前 `paneId` 和原始 command。
2. 空白、无 pane 或正在提交时直接返回。
3. 调用新增 command API。
4. 成功：清空输入、刷新 state、恢复焦点。
5. 失败：保留文本、显示现有错误条、恢复焦点。

keydown：

```javascript
if (event.key === "Enter" && !event.isComposing) {
  event.preventDefault();
  elements.commandComposer.requestSubmit();
}
```

确保中文输入法组合态不会误提交。

**Step 4：让 running 命令立即显示**

删除消息列表中“只显示终态 command”的过滤条件。命令卡行为改为：

- `requested` / `approved` / `running`：显示原命令和“正在运行…”。
- `completed` / `failed` / `rejected` / `incomplete`：显示现有终态元数据和
  完整输出。
- Hub 用同一 record ID 更新后，轮询自然把原卡替换为终态，不新增第二张卡。

不要改变 raw input/special key 卡片与操作日志分类。

**Step 5：验证**

Run:

```powershell
cargo test --test web_ui
```

Expected: 全部静态 UI 测试通过。

**Step 6：提交**

```powershell
git add web/index.html tests/web_ui.rs
git commit -m "feat: add command composer to message mode"
```

---

### Task 5：切换为白色 ChatGPT 风格

**Files:**

- Modify: `web/index.html`
- Modify: `tests/web_ui.rs`

**Step 1：写主题失败测试**

在 `tests/web_ui.rs` 断言：

```rust
assert!(PAGE.contains(r#"name="color-scheme" content="light""#));
assert!(PAGE.contains("--bg: #ffffff"));
assert!(PAGE.contains("--panel-2: #f7f7f8"));
assert!(!PAGE.contains("--bg: #0b0d10"));
```

同时保留安全渲染和可访问性断言。

Run:

```powershell
cargo test --test web_ui
```

Expected: 失败，当前仍是 dark theme。

**Step 2：只改 CSS，不改信息架构**

基础变量：

```css
--bg: #ffffff;
--panel: #ffffff;
--panel-2: #f7f7f8;
--line: #e5e7eb;
--text: #202123;
--muted: #6b7280;
--blue: #2563eb;
--cyan: #10a37f;
--amber: #b7791f;
--red: #dc2626;
--green: #10a37f;
--shadow: 0 8px 24px rgb(0 0 0 / 8%);
```

视觉规则：

- body/main/header 用白底，移除径向深色背景。
- sidebar 与次级面板用 `#f7f7f8`。
- active mode 使用深色底和白字。
- 消息列最大宽度约 880px 并居中。
- 人工命令卡用浅灰气泡，输出卡白底、浅分隔线。
- composer 固定在消息区底部，白底圆角轻阴影，发送按钮黑底白字。
- 交互快照容器也使用白/浅灰底，不保留整块深色页面。
- dialog、日志、Gate 和 toast 改为白底；状态颜色语义保持。
- 键盘 `:focus-visible` 保持清晰。
- 小屏布局继续可用。

用 `rg` 检查并替换遗留的硬编码深色背景，但不改 terminal 文本、命令输出或
用户数据。

**Step 3：验证主题契约**

Run:

```powershell
cargo test --test web_ui
```

Expected: 全部通过。

**Step 4：提交**

```powershell
git add web/index.html tests/web_ui.rs
git commit -m "style: use a light chat console theme"
```

---

### Task 6：文档、完整验证与本机替换

**Files:**

- Modify: `README.md`
- Verify: all changed files
- Runtime: worktree release binary and existing local hub

**Step 1：更新 README**

在 web console 说明中明确：

- Messages 模式底部可提交完整单行命令。
- 命令来源显示为“你”，同一张卡从运行中变为干净终态。
- raw key/send-keys 只显示输入，不承诺输出归属。
- Interactive 模式保持屏幕快照与逐键转发。

Run:

```powershell
rg -n "Messages|Interactive|command|命令" README.md
```

Expected: 新边界清晰，无“所有逐键输入都能得到归属输出”的错误承诺。

**Step 2：格式与聚焦测试**

Run:

```powershell
cargo fmt --check
```

Expected: exit 0。

Run:

```powershell
cargo test --test web --test web_ui --test control_client
```

Expected: exit 0，所有相关测试通过。

Run:

```powershell
cargo test commands
```

Expected: exit 0，tracker 现有测试通过。

**Step 3：全量质量检查**

Run:

```powershell
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: exit 0，无 warning。

Run:

```powershell
cargo test --all-targets
```

Expected: 新增与受影响测试全部通过。若仍出现已确认的 Windows/环境基线失败，
逐项与修改前基线对比并如实记录；不得把基线失败描述成此次通过。

Run:

```powershell
cargo build --release
```

Expected: exit 0，生成新的 release binary。

**Step 4：重启本机 hub**

- 只停止当前 worktree release 的 `--web` 进程。
- 用同一 SSH/config/socket 参数启动新 release binary，继续监听
  `127.0.0.1:38473`。
- 不改 Gate 文件；确认 Gate 仍为关闭。
- 不安装服务、不增加开机启动。

**Step 5：独立 tmux pane 浏览器验收**

创建临时 window/pane，避免污染现有工作 pane。在浏览器消息模式：

1. 选择临时 pane。
2. 在 composer 输入 `printf 'WEB_COMPOSER_OK\n'`。
3. 按 Enter。
4. 立即看到来源“你”的单张运行中命令卡。
5. 同一卡更新为 completed、exit code 0，并显示
   `WEB_COMPOSER_OK`，无 tmux MCP 包装文本。
6. 输入框已清空并保持可继续输入。
7. 连续提交两条短命令，确认同 pane 按 tracker 顺序完成。
8. 切到 Interactive，确认原逐键和快照体验未回归。
9. 截图或 DOM 检查白色主题、侧栏、composer、审批框和移动布局。

验收后删除仅用于测试的临时 window/pane，并报告删除是有意且不可恢复的
临时资源清理。

**Step 6：最终自检**

Run:

```powershell
git diff --check
```

Expected: exit 0。

Run:

```powershell
git status --short
```

Expected: 只包含本计划的预期改动；无构建产物、token 或本机配置入库。

逐条对照
`docs/superpowers/specs/2026-07-24-message-console-design.md`，确认没有加入
PTY、WebSocket、screen-diff 推断或跨客户端调度器。

**Step 7：提交文档和最终必要修正**

```powershell
git add README.md
git commit -m "docs: explain command-first message mode"
```

若验证阶段产生必要代码修正，先用对应失败测试复现，再单独提交，不把无关改动
混入文档提交。
