# 2026-10-02 unsubmitted-input guard, system OpenSSH, readable resource list

- **Unsubmitted-input guard.** The action log (2026-09-02 → 10-01) has 46 `paste-text` calls,
  each one complete command (mostly heredoc file writes); none was meant to be joined with a
  later paste. Some shells submit a pasted trailing newline, others (bracketed paste) keep the
  text on the line, where the next typed command is appended (reproduced on a real host).
  Now, after input that can leave text on an idle pane's command line (`paste-text`, Enter-less
  `send-keys`/`send-hex`, keys other than Enter/Ctrl-C), `execute-command` and another
  `paste-text` on that pane are refused until Enter or Ctrl-C — both harmless on an empty line.
  Input to a running tracked command is not tracked. Only input from this MCP process is known.
- **System OpenSSH on Windows.** All ssh spawns use `C:\Windows\System32\OpenSSH\ssh.exe` when
  present (Git's MSYS ssh hangs under this process even when first on PATH);
  `TMUX_MCP_SSH_PROGRAM` overrides. Verified with Git's ssh first on PATH.
- **Resource list.** `resources/list` emitted URIs without a target (unreadable since v0.6.1)
  and enumerated topology via local tmux. It now lists only target-qualified, readable entries:
  per-target `server/info` and `clients`, plus tracked command results. The clients template is
  `tmux://{target}/clients`.

# 2026-10-01 SSH sessions, transport resilience, write-file, scripts

Built and verified with `--target-dir target/test-build`, then installed as the root
`tmux-mcp.exe` on 2026-10-02 (previous binary in `target/deployment-backups/`). Connected clients
load it after reconnecting. Evidence and measurements: `.planning/2026-10-01-ssh-efficiency/`.

- **Measurements that drove this.** In the 2026-09 action log, a fresh SSH per subprocess cost
  p50 4.9–7.0 s; `read-file` used 2 SSH processes and `execute-command` 3–5. A live `ssh -vv`
  showed ~5.8 s of handshake for ~0.5 s of work, and a lighter key exchange gave no consistent
  gain. 40 of 44 `tracking_error` pauses were `wait-for failed: Connection reset`.
- **Persistent SSH sessions (`src/ssh_pool.rs`).** Up to 4 idle sessions per ssh argv (so
  targets/accounts never share one) run a bash dispatcher: one base64 line in, one line out.
  Requests run through the login shell like one-shot ssh, with stdin/stdout/stderr redirected to
  private temp files. A missing bash/base64/mktemp marks the host unsupported for 10 min and falls
  back to one-shot ssh. A session that died while idle is retried only for read-only requests; a
  modifying request first pings a session idle > 20 s and is never resent. Request budgets now
  measure remote work; connection setup has its own 30 s budget (one-shot ssh gets +20 s).
  `TMUX_MCP_SSH_POOL=0` disables sessions. Timing records use transports `ssh-pool`/`ssh-pool-new`.
  Real-server read-only check: capture-pane/read-file 0.3–0.9 s vs 4–11.5 s.
- **Watcher resilience.** When the `wait-for` watcher's connection drops, tracking continues by
  polling the durable exit buffer (2→30 s backoff). Only a vanished pane/tmux server is a
  tracking error. Exit-buffer reads after the signal retry transport failures.
- **`write-file`.** Atomic temp + `ln` (create, never clobbers) or `mv -f` (overwrite, keeps
  mode); refuses symlinks and non-regular files; ≤ 512 KiB; modifying (Gate) and gated by
  `allow_execute_command`.
- **`execute-command` `script`.** Exactly one of command/script; no detach/delayMs; ≤ 64 KiB;
  CRLF normalized, NUL/lone CR rejected; checked by the command policy (fails closed under a
  filter). Uploaded out of band (umask 077) and `bash -n` checked before anything is typed; the
  pane receives only the tracked line `bash '<file>'`. Scripts run in a child bash and are kept
  7 days.
- **`notify`.** With `--claude-channel`, a command finishing after the call returns is announced
  once on the channel (missed broadcast events are reconciled). `call_tool` now records the
  client peer; before, channel messages (GPU watches too) only worked after a resource request.
- **Real-server modifying E2E (throwaway session on milab-seven-intern, cleaned up):** atomic
  create/refuse/overwrite/read-back, script exit code 3 with heredoc/`&`/comments, `cd` and `exit`
  not leaking into the pane, `bash -n` rejection before typing, channel completion notice.
- **press-special-key aliases** such as `ctrl-c`, `C-c`, `^c`, `esc`, `pgdn`.
- **Truncation guards.** A dropped one-shot connection can end stdin early. `write-file` and
  script upload compare the received size with the sent length before installing anything;
  `paste-text` compares the staged tmux buffer byte-for-byte before pasting, so a cut heredoc or
  command is never typed. Pooled requests run only as complete lines.
- **Overwrite scope.** No ownership rule (user decision 2026-10-02: a personal tool must not
  limit its own writes): with `overwrite`, any regular file in a writable directory is replaced,
  keeping its mode. Script-cache pruning still touches only the account's own uuid-named files
  in a real, owned directory.
- **Review fixes (2026-10-02).** Relative `write-file` paths get a `./` prefix, so a name such
  as `-tother` is created literally instead of being parsed as `ln -t other`. A lost or garbled
  reply is reported as "Write result uncertain … check before retrying"; "File not written" is
  reserved for refusals the remote script reported before touching the target. After a
  `wait-for` drop, polling finishes a command once its exit buffer exists and START is no longer
  in the capture (long output), instead of keeping the pane leased forever; START visible
  without DONE still waits. Command allowlists do not restrict `write-file` (by decision).
- **Test harness.** Stub scripts run through `sh` in test builds; `call_tool` tests default to a
  test target instead of depending on another test's `TMUX_MCP_SSH`. The suite now passes
  completely on Windows (lib 238, bin 385): the pane-info stub matches the escaped format,
  resource policy tests call the target-scoped handler and expect `tmux://{target}/…` templates,
  assertions follow the 2026-09-22 messages (uncertain delivery, no command echo), the
  final-capture timeout test has room for slow stub spawns while still failing without the
  bound, and the local `realpath` comparison is Unix-only. Trivial tests were removed (resource
  happy paths, string-shape checks, alias enumeration).

# 2026-09-22 implementation and decisions

These changes are installed in the root `tmux-mcp.exe` as v0.6.1. Existing client processes were preserved and load the new binary when reconnected. The old executable is backed up locally under target/deployment-backups. No remote tasks or SSH configurations were changed.

## Measurements before connection reuse

MCP tool dispatches now attach `timing` to their existing `ActionRecord` in the Web control hub's bounded `%LOCALAPPDATA%\tmux-mcp\events.jsonl` log. Keep the hub running and keep `--web-url` configured to persist these measurements. Without that connection, there is no durable timing dataset; this is not a second independent logging service.

Each measured call records target alias, dispatch duration, time before dispatch (preflight/Gate/logging), caller waitMs, wait-timeout flag, response outcome, and up to 128 foreground subprocess measurements. Each subprocess records a fixed operation label, SSH/local transport, queue and total elapsed milliseconds, outcome, and retained stdout bytes. No shell text, file paths, credentials, or subprocess argument strings are added to timing metadata. Existing action arguments/results retain their previous logging behavior.

Instrumented paths: ordinary tmux adapter calls and bounded file/Git/GPU commands. Background command watchers and GPU watches are deliberately excluded from foreground latency. Resource reads, separate buffer-to-file transfers, and Web UI actions are not a complete transport census. Calls rejected before tool dispatch retain existing action records but no dispatch timing. Subprocess sums can exceed wall time because calls overlap. Process duration includes SSH setup plus remote execution, not an isolated handshake measurement.

```powershell
.\scripts\summarize-timings.ps1
.\scripts\summarize-timings.ps1 -Json
.\scripts\test-summarize-timings.ps1
```

The summary deduplicates lifecycle updates by action ID and groups samples by target/tool. It reports count, response errors, caller timeouts, P50/P95/max, subprocess/SSH counts, queue/process sums, and omitted detail. Legacy records without timing and incomplete log lines are counted separately. This bounded log represents retained samples, not lifetime totals. Explicit waitMs calls are expected to have high wall time; compare their transport times separately.

Connection reuse remains disabled. A future A/B comparison should use the same fixed read-only MCP operations and target accounts, compare cold and warm connections separately, and preserve the same routing/policy/tmux semantics. Never combine admin and intern connection identities. Any pool must include at least host, port, user, and authentication/config identity. It must not expose a generic SSH command tool. Remote tmux still owns long-running jobs independently of the transport connection.

## Implemented behavior

- `target` is declared required as well as checked at runtime. Command records retain their target; pane occupancy, queries, and purging distinguish identical pane IDs on different targets.
- Creation tools briefly instruct agents to inspect/reuse their task's pane, create only for necessary concurrency, and never bypass busy/error/timeout by creating another window. Empty state returns an explicit creation hint. There is no automatic session creation or automatic killing of existing panes.
- Actual background `&` is rejected with or without spaces; `&&`, `&>`, file-descriptor redirection, quoted/escaped ampersands and Bash `|&` remain supported. Actual shell comments remain rejected in tracked mode.
- `waitMs` is capped at 110000; handler preparation is deducted from that budget. Recovery/capture is inside the remaining wait budget, and expiry returns cached state without a new capture. Completion and pane release are committed together before best-effort remote buffer cleanup. Caller timeout never kills the task. This is not a deadline on human Gate approval, network delivery, or the remote command's lifetime.
- `file-stat`: required target/paneId/path, optional socket. Returns type, byte size, and modification time. Linux targets use fixed GNU stat arguments; symlinks themselves are inspected, not followed. Local mode uses filesystem metadata. No recursive directory scan.
- `gpu-snapshot`: required target; no pane/session needed. Uses fixed bounded nvidia-smi inventory and compute-process queries, returns CSV headers/units and truncation flags. Inventory failure is an explicit tool error, not idle capacity. Process-query failure retains GPU information with a diagnostic. No GPU changes or watcher is started. Each query has a 10-second execution timeout, a 10-second queue timeout, and a 64 KiB stdout bound.
- Both snapshot tools are in the default tool group, honor tool policy, and bypass approval only as read-only tools, like existing file/Git queries. `du` is deferred.

## Confirmed boundaries

Command echo: execute/result tool snapshots omit the duplicate command field by default. verbose:true includes it; command result resources retain it. Execution and existing audit arguments are unchanged.

Pane safety: reuse one known-ready task-owned pane. A dispatch failure or tracking_error retains its record and ownership; subsequent MCP changes on that target stop with an instruction to report to the user. Reading state/output does not clear the protection. Interactive input failures set the existing AI pause, which also permits read-only inspection. Never automatically clear input, send Ctrl-C, create a replacement window or replay an ambiguous command. The human can inspect and decide recovery later. No new automated recovery or prompt/input-buffer heuristics are introduced.

The tracker is in memory, scoped to this MCP process. It does not detect arbitrary human input, coordinate separate MCP processes, or prove an unfamiliar pane is empty after reconnect. Agents must not adopt an unfamiliar pane or resume an unresolved task without checking with the user. Clearing the Web pause does not itself resolve a retained tracking_error record.

Detach: keep detach:false by default. detach:true skips completion tracking, not pane occupancy or raw-mode policy. It keeps the pane occupied and reports that no automatic completion result is available. waitMs does not wait for an impossible detached completion. Normal tracked execution already supports long-running work inside tmux.

Output logs (item 7): full-output persistence is outside this MCP's scope. For long/verbose work, arrange application logging before launch and use existing read-file for bounded inspection. Truncation means some output may be unavailable; never rerun a command merely to recreate its output. No saveOutput option, automatic remote log files, quotas, TTL, or collector is implemented.

SSH connection reuse (item 3) stays deferred. Retained legacy logs have no timing samples; no speedup is claimed. Collect measurements before evaluating reuse. No standalone SSH experiment or remote task change was performed.

Wrapper filtering remains presentation-only. raw:true returns unfiltered pane output; no remote helper is installed. Recursive disk usage queries remain deferred.

## Validation and activation

Native Windows checks cover compilation, clippy, parser/policy regressions, timing persistence/statistics, target isolation, wait budgets, uncertain-pane guards, detach defaults/policy, and CLI/control/Web UI tests. POSIX tmux integration and real GPU/SSH behavior are not validated on this Windows host. Local binary replacement does not reconnect an existing client's stdio session: reconnect/restart the client to load the new program.
