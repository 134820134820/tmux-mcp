# 2026-10-01 current work — installed 2026-10-02

Active plan: [.planning/2026-10-01-ssh-efficiency/task_plan.md](.planning/2026-10-01-ssh-efficiency/task_plan.md) (start with its "Next Step", then `progress.md` "Resume here" and `findings.md`). The new build is installed as root `tmux-mcp.exe`. Codex review fixes (Phase 10) are in source and tested but not installed; other open items are user decisions. Previous work (2026-09-22) is complete; historical content below.

# Task Plan: Separate timeouts from tracking errors

## Goal
Make ordinary wait/deadline expiry report timeout state without falsely reporting an internal tracking failure, while preserving real side-channel/capture errors and all current uncommitted logging work.

## Current Phase
Phase 9: SIGINT-safe tracked command finalization (local only)

## Phases

### Phase 1: Trace semantics
- [x] Find every `TrackingError` producer, serializer, UI mapping, and terminal-state consumer
- [x] Inspect recent local records without exposing command/output content
- **Status:** complete

### Phase 2: Implement the shared fix
- [x] Turn the background deadline into a cleanup checkpoint without cancelling the authoritative wait
- [x] Keep `waitMs` expiry non-terminal and non-error
- [x] Normalize future `tracking_error` serialization while accepting legacy `trackingerror`
- [x] Add the smallest regression coverage
- **Status:** complete

### Phase 3: Validate locally
- [x] Run format, focused tests, Clippy, and diff checks
- [x] Build a staged release without replacing the running executable
- **Status:** complete

### Phase 4: Report
- [x] Explain the corrected semantics and activation boundary
- **Status:** complete

### Phase 5: Audit command-output transport
- [ ] Trace every output producer/consumer and the exact `capture-pane` limits
- [ ] Compare safer alternatives without touching remote state
- [ ] Produce a staged recommendation and define the smallest next change
- **Status:** paused for requested deployment

### Phase 6: Deploy and commit the previous fix
- [x] Verify exact installed/staged binaries and related process targets
- [x] Back up and replace the installed binary
- [x] Stop only the clearly identified repository MCP processes before replacement
- [x] Commit the outstanding product changes
- **Status:** complete (replacement and local commit complete; push not requested)

### Phase 7: Diagnose web “connecting” latency
- [x] Trace the page connection state and first-load request path
- [x] Identify whether delay is frontend polling, backend topology/SSH work, or stale process state
- [x] Report the smallest safe fix and validation next step
- **Status:** complete (diagnosis only; no source or remote changes)

### Phase 8: Optimize topology and remote command latency
- [x] Add a shared bounded topology snapshot path for web and MCP state tools
- [x] Remove N+1 remote reads from the measured web/state paths; preserve policy filtering and existing schemas
- [x] Bound shared SSH/tmux subprocesses without changing remote command semantics
- [x] Audit and minimize design-induced waits in `capture-pane`, `send-keys`, and `execute-command`
- [x] Add focused local regression/performance-shape tests and run build checks
- **Status:** complete (local implementation, non-remote validation, and deployment; remote measurement deferred)

### Phase 9: SIGINT-safe tracked command finalization
- [x] Reproduce signal termination using isolated local shells and select the smallest correct wrapper
- [x] Add regression coverage and implement completion that preserves shell state
- [x] Run focused checks and build a staged release
- Do not stop running MCP processes, replace the installed executable, or contact remote servers.
- **Status:** complete (local repair and staged build; real Linux tmux execution remains unverified)

## Constraints and Decisions
| Decision | Rationale |
|----------|-----------|
| Preserve current README/Web-log changes | They are the user's unfinished work from the preceding request |
| Never map a client wait budget to command failure | The command may still be running after the caller stops waiting |
| Keep genuine protocol/capture failures distinct | Hiding real loss of command observability would be unsafe |
| No tmux, SSH, remote calls, binary replacement, commit, or push | None is required to fix and validate local state semantics |
| Do not add a `TimedOut` terminal status | It would stop tracking a potentially live command and either reject later calls globally or release the pane unsafely |
| Pin one side-channel wait across periodic cleanup checks | This preserves signals, lets long commands finish normally, and allows purged records to end the waiter |
| Add `kill_on_drop` to the waiter process | Dropping a purged/server-shutdown wait must not leave a local tmux/SSH client behind |

## Errors Encountered
| Error | Attempt | Resolution |
|-------|---------|------------|
| Planning catch-up could not access uv's user cache | 1 | Read Git status, diff stat, and planning files directly; do not retry |
| UI status search regex was malformed by nested quoting | 1 | Switch to fixed-string patterns; do not retry the regex |
| Rustfmt check found one mechanical line-wrap difference | 1 | Run rustfmt once, then rerun the check |
| Focused watcher test could not execute the repository's extensionless POSIX fake `tmux` on native Windows | 1 | Use an ignored target-local Windows proxy for this one regression; never call real tmux |
| Full library run retained 12 existing Windows/POSIX-fixture failures | 1 | Record the platform limitations; relevant regressions passed, so do not expand scope into unrelated fixture repairs |
| Final multi-pattern `rg` was mangled by nested PowerShell quoting | 1 | Use separate fixed-string searches for source line references; do not retry the regex |
| CIM process inventory was denied by Windows permissions | 1 | Use the narrower `Get-Process`/tasklist views and exact executable-path checks; do not broaden privileges yet |
| RTK condensed PowerShell hash-object output to unusable lines | 1 | Re-run hashes with explicit formatted strings before making replacement decisions |
| First WMIC process query used an invalid `process` alias form | 1 | Use `path Win32_Process` or host-owned process metadata; no retry of the malformed syntax |
| Deployment findings patch initially targeted a line in the wrong planning file | 1 | Re-read the exact headers and apply separate hunks; no product files were changed |
| Stopping the two exact old MCP PIDs was denied in the sandbox | 1 | Request one elevated, path-validated stop/replace transaction; keep the Codex host excluded |
| Old MCP children did not auto-respawn after replacement | 1 | Inspect host/restart boundaries; do not launch a disconnected stdio orphan or kill the Codex host blindly |
| Read-only MCP state probe failed because the transport was already closed | 1 | Record that the host needs an explicit session/connection restart; inspect local Codex lifecycle commands without remote calls |
| Planning patch bundled two operations for one file | 1 | Split the plan update into one apply-patch operation; no product files were changed |
| One source search used an invalid PowerShell glob (`src/*.rs`) | 1 | Use explicit file paths with `rg`; no runtime or repository state was affected |
| Cargo manifest inspection omitted the `pwsh` wrapper | 1 | Do not retry the bare command; source and lockfile searches already established the relevant dependency fact |
| A multi-file planning patch mixed a `findings.md` context with `progress.md` | 1 | Split the correction by file and apply exact contexts; no product files were changed |

---

# Archived Plan: Bound local agent-call logging

## Goal
Keep the standard local state directory, make `events.jsonl` predictably bounded, and preserve enough structured data to diagnose agent tool usage without touching remote state.

## Current Phase
Complete

## Phases

### Phase 1: Audit the shared persistence path
- [x] Trace record-size handling, compaction, recovery, and existing tests
- [x] Choose the smallest shared fix that covers every tool record
- **Status:** complete

### Phase 2: Implement bounded persistence
- [x] Retain the existing 1 MiB transport bound instead of adding an arbitrary smaller content cap
- [x] Make the file-size budget a true post-compaction ceiling
- [x] Add focused regression tests
- **Status:** complete

### Phase 3: Validate locally
- [x] Run rustfmt and focused Web/control-client tests
- [x] Run Clippy and final diff/static checks
- [x] Confirm no remote/tmux execution occurred
- **Status:** complete

### Phase 4: Report
- [x] Explain why LocalAppData is the correct default
- [x] State behavior, limits, and restart boundary
- **Status:** complete

## Decisions Made
| Decision | Rationale |
|----------|-----------|
| Keep `%LOCALAPPDATA%\tmux-mcp` | It is Windows' standard machine-local application-state location and avoids repository pollution or accidental commits |
| Reuse the existing centralized Web Hub writer | It already serializes concurrent agent writes and has crash-safe compaction |
| Do not add a second tracing/logging subsystem | The existing structured event stream already captures the needed call lifecycle |
| Do not touch remote tmux/SSH state | The requested change is entirely local persistence behavior |
| Keep the existing 1 MiB request cap | Supported read tools already cap payloads at 256 KiB; a new 64 KiB cap would discard useful diagnostics without solving the demonstrated total-size issue |
| Enforce the 4 MiB budget during compaction | A bounded newest suffix preserves the most relevant calls and provides a real disk-size guarantee |

## Errors Encountered
| Error | Attempt | Resolution |
|-------|---------|------------|
| Session catch-up invoked the inaccessible Windows Store `python.exe` alias | 1 | Read the existing planning files and Git state directly; do not repeat that path |
| Git rejected the workspace ownership on the first status call | 1 | Use per-command `-c safe.directory=E:/buyi_work/tmux-mcp`; do not change global Git config |
| First planning patch tried to delete and add the same file in one patch | 1 | Preserve the completed plan below as an archive and prepend the new active plan |
| First phase-update patch targeted an error row in the wrong planning file | 1 | Split the updates by file and patch each exact section |
| First phase-update patch targeted an error row in the wrong planning file | 1 | Split the updates by file and patch each exact section |

---

# Archived Plan: Fix tracked command output readiness

## Goal
Make the tracked-command API wait for its bounded final output capture before reporting a ready result, without touching remote state or running tmux.

## Next Step
No product work remains; local planning files and executable backups may be retained or cleaned separately.

## Current Phase
Phase 7

## Phases

### Phase 1: Reconfirm shared call path
- [x] Re-read tracker state, watcher ordering, and every wait caller
- [x] Select the smallest readiness boundary that fixes all public paths
- **Status:** complete

### Phase 2: Implement root fix
- [x] Change shared tracker readiness semantics
- [x] Add one focused regression test
- **Status:** complete

### Phase 3: Align agent-facing contract
- [x] Return the ready snapshot from the existing wait path if compatible
- [x] Correct only directly misleading workflow text needed by the fix
- **Status:** complete

### Phase 4: Validate locally
- [x] Run formatting/tests if a local Rust toolchain is available (not available)
- [x] Otherwise perform diff and static call-site checks
- **Status:** complete

### Phase 5: Report
- [x] Summarize changed behavior and activation boundary
- [x] State validation limits and remaining pane-history ceiling
- **Status:** complete

### Phase 6: Build and activate on disk
- [x] Locate an available local Rust build path
- [x] Run formatting and the non-remote test suites
- [x] Build a release binary and validate it before replacement
- [x] Back up the old executable and replace it atomically
- [x] Verify the installed binary without restarting existing MCP processes
- **Status:** complete

### Phase 7: Commit and push
- [x] Remove temporary generated test adapters
- [x] Run final formatting, lint, and focused regression checks
- [x] Stage only product source/tests/docs and `tmux-mcp.exe`
- [x] Commit and push `main` to `origin`
- **Status:** complete

## Key Questions
1. Can one shared readiness signal cover tool waits and resource notifications?
2. Can the execute response include output without breaking its public schema?
3. Which limitation is fundamental to pane history and must remain explicit?

## Decisions Made
| Decision | Rationale |
|----------|-----------|
| Fix locally only; do not restart any running service | Existing agents must remain unaffected |
| Do not use remote execution or tmux | The root cause is in local tracker ordering and tests |
| Prefer one shared readiness boundary | Fixing every caller separately would be larger and easier to regress |
| Add one internal final-capture-ready bit | Terminal status must remain side-channel-authoritative, but waiters need a separate presentation boundary |
| Keep pane ownership through final capture | Releasing it earlier lets the next tracked command destroy the output being recovered |
| Emit Terminal only when final capture attempt is done | One notification should mean the resource snapshot is ready to read |
| Reuse `CommandSnapshot` in optional execute result | Avoid duplicating output/status fields when `waitMs` completes in one call |
| Replace the installed executable only after a successful staged build | A failed build must not disturb the currently installed binary |
| Do not restart existing MCP processes during build validation | Windows processes already holding the old image can continue until the user restarts them |
| Exclude planning files and executable backups from the commit | They are local operational artifacts, not product source or release content |

## Errors Encountered
| Error | Attempt | Resolution |
|-------|---------|------------|
| `uv` could not access its default cache while running session catch-up | 1 | Do not repeat; continue from confirmed absence of existing planning files |
| A complex `rg` regex for the command resource branch was malformed | 1 | Use fixed-string/line-slice lookup next; do not repeat the regex |
| Tracking-doc search named a nonexistent `config.toml.example` | 1 | Restrict future searches to discovered files; no retry needed |
| Git status warned that the sandbox cannot read the user-level global ignore file | 1 | Status still succeeded; no retry required |
| Local Cargo was not available on PATH | 1 | Honored remote-safety constraint and completed static/test-code validation only |
| Session catch-up initially could not access the sandboxed uv cache | 1 | Re-ran the read-only script with approved user-cache access; it succeeded |
| Winget reported no applicable Rustup installer with `--scope user` | 1 | Package metadata shows the official per-user `rustup-init.exe`; retry without the unsupported scope filter |
| Planning update patch contained an extra hunk marker | 1 | Corrected the patch structure without rereading or changing code |
| Release phase was marked complete before staged binary smoke checks | 1 | Returned it to pending immediately; run metadata/help/version checks before completion |
| Windows refused `Move-Item -Force` over the running installed executable | 1 | Backup and verified `.new` remain; try a same-directory rename-and-place transaction with rollback, without stopping processes |
| Commit-phase planning patch contained an extra hunk marker | 1 | Corrected the patch structure; product files were unaffected |
| Push to `origin/main` was rejected pending explicit destination/content confirmation | 1 | Do not retry or route around review; ask the user to explicitly approve publishing source/tests/docs and the Windows binary to the named GitHub repository |

# Active Plan: Unified multi-target SSH MCP
- [x] Normalize local aliases and key names to milab-<machine>-<role>
- [x] Add target config schema and startup loading
- [ ] Add required target to public tool inputs
- [x] Route tmux/tracker/resource calls through target alias
- [x] Replace single --ssh process model and update docs/tests
- [x] Build and test isolated Web adaptation without touching running MCP processes


## Session 2026-09-13: defer next-version MCP optimizations

The following are explicitly deferred until the next version and are not part of the currently used binary: remote helper wrapper, SSH connection reuse, complete wait/result lifecycle repair, recoverable remote output files, detached long-running command support, and stat/du/nvidia-smi snapshot tools. The Web multi-target adaptation is implemented in the uncommitted source and separate test build; the currently running root binary has not been replaced. The adapted Web process accepts an optional `--ssh <alias>` for its default target and otherwise uses the first `targets.toml` entry, then the UI switches among the entries.

### Status labels
- **已确定/已实施未提交:** target inventory and local aliases, one-MCP client registrations, README and TOOL_SURFACE documentation alignment, Web target selector and per-target routing in source, optional Web `--ssh` default with first-target fallback, unused executable backup cleanup.
- **已确定/当前已使用:** root `tmux-mcp.exe` and the running MCP processes; they still run the pre-Web-adaptation binary and were not replaced.
- **下个版本待商议:** helper wrapper, SSH connection reuse, wait/result state repair, recoverable output files, detached commands, and stat/du/nvidia-smi tools.

### MCP 优化建议 1-9：逐条状态

1. **回显噪音 / 远端 helper wrapper**
   - **已确定:** 需要减少 wrapper 在 pane 中产生的上下文噪音。
   - **已实施未提交:** 仅有 `capture-pane` 默认过滤 `TMUX_MCP_` 和 `__tmux_mcp_` 行的源码改动。
   - **当前使用版本:** 仍使用旧的长 wrapper，过滤改动未部署。
   - **需要商议:** 远端 helper 的安装位置、shell 兼容方式、版本更新和回退策略。

2. **响应中重复返回 command 原文**
   - **已确定:** 默认响应不需要原始 command。
   - **已实施未提交:** 增加 `verbose` 字段，默认隐藏 command 的源码改动。
   - **当前使用版本:** 仍返回原 command，未部署。
   - **需要商议:** `verbose` 是否同时适用于 `execute-command` 和 `get-command-result`，以及资源 URI 是否保留 command。

3. **SSH 重建连接和轮询慢**
   - **已确定:** 多 target 不能共用不同账号的连接状态；连接复用必须按 target/账号隔离。
   - **已实施未提交:** 无。
   - **当前使用版本:** 仍可能重复建立 SSH 连接，未完成复用。
   - **需要商议:** 先采用 OpenSSH ControlMaster，还是实现常驻 `ssh -tt tmux -C` 控制连接。

4. **waitMs 语义和 110 秒上限**
   - **已确定:** 已退出的命令应在同一响应中尽量完成最终输出收集；服务端等待不能超过 Claude 的硬上限。
   - **已实施未提交:** 增加 110 秒上限的源码改动；等待完成状态的完整修复尚未完成。
   - **当前使用版本:** 仍可能出现 `completed + resultReady:false`，未部署。
   - **需要商议:** 最终 capture 的额外宽限时间和超时快照字段。

5. **target、list-targets、备注和新 session 发现性**
   - **已确定:** 目标清单包括 7/8 号机 admin/intern 和 10 号机 admin；10 号机不加入 intern。
   - **已实施未提交:** 目标清单、备注、动态 target schema、`list-targets` 默认工具组和缺少 target 时的提示均有源码改动。
   - **当前使用版本:** 多 target 路由和 `targets.toml` 已在使用；默认工具列表仍隐藏 `list-targets`，target schema 仍未部署。
   - **需要商议:** `list-targets` 是否始终放在默认工具组，以及新账号无 tmux 时由 MCP 自动创建还是只提示。

6. **tracked 命令中的 shell 运算符判断**
   - **已确定:** 应放过 `2>&1`、`&&`、`&>` 和无空格的普通字符组合，只拦真正的后台 `&`。
   - **已实施未提交:** 已修改判断逻辑并添加回归测试。
   - **当前使用版本:** 仍使用旧判断逻辑，改动未部署。
   - **需要商议:** 是否继续拒绝未引号的 `#` 和其他 shell 控制语法；本条只涉及 `&`。

7. **输出截断和恢复**
   - **已确定:** 仅依靠 `capture-pane` 无法恢复被截断的完整 stdout。
   - **已实施未提交:** 无完整实现。
   - **当前使用版本:** `outputTruncated:true` 时没有可恢复的完整输出接口。
   - **需要商议:** 远端临时文件目录、单命令大小上限、TTL、分段读取按字节还是按行。

8. **长驻进程占用 pane**
   - **已确定:** 长驻进程需要绕过 tracked wrapper 和 busy 状态。
   - **已实施未提交:** 增加 `detach:true` 参数的源码改动。
   - **当前使用版本:** 不支持该参数，改动未部署。
   - **需要商议:** detached 命令是否仍记录 commandId，以及退出状态是否只通过 capture 或远端文件查看。

9. **状态错误提示和 stat/du/nvidia-smi 快照**
   - **已确定:** 缺少 target 时应直接列出可用 target；巡检不应绕道 tracked `execute-command`。
   - **已实施未提交:** 缺少 target 时列出目标的源码改动。
   - **当前使用版本:** 7/8/10 的状态仍需猜 target；没有独立 `stat`、`du`、`nvidia-smi` 工具。
   - **需要商议:** 三个工具的默认输出上限、超时、`du` 深度和 GPU 不可用时的返回格式。
