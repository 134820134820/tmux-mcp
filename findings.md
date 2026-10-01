# Findings & Decisions

## SIGINT tracking repair (2026-09-08)
- Confirmed HEAD d7b6709 is clean apart from existing local planning files/backups.
- The wrapper's trailing side-channel operations can be skipped by SIGINT; the watcher and retention logic keep occupied Running records indefinitely.
- Ctrl-C delivery is not proof of exit; do not release the pane from the key sender.
- Git for Windows provides a local Bash at C:/Program Files/Git/bin/bash.exe; use isolated local processes for shell-behavior tests, never configured SSH/tmux.
- User permits local implementation but explicitly excludes changes to currently running processes and all remote operations.
- Local Git Bash reproduced the missing tail after one child SIGINT. A no-op INT trap repairs a simple child but still loses the tail when interrupted inside an eval loop; do not ship the simplistic trap patch.
- Investigating a one-command Bash PROMPT_COMMAND fallback: finalize only after the shell returns to its prompt, preserve existing INT handling and the command's current-shell environment.
- No-op trap + eval loses loop completion in the local interactive-script probe. Pipe-driven Git Bash has no controlling terminal and behaves differently; a real PTY regression is needed for Linux job-control coverage.
- Explicit SIGINT to the isolated parent shell and child reproduces the skipped tail in pipe-driven Bash. A temporary PROMPT_COMMAND callback then finalizes with 130, restores scalar/array prompt hooks, and leaves INT traps unchanged. Implement this only for actual Bash (runtime BASH_VERSION guard); retain legacy sh/dash/zsh/fish handling.

## Capture dependence audit (2026-09-02)
- New user question: command results rely heavily on `capture-pane`; assess better transport options before changing code.
- Scope is local read-only audit; do not execute real tmux/SSH or modify remote state.
- The tracker has two output paths: running-command refresh and terminal finalization both call `capture_pane`; the private `wait-for` channel supplies lifecycle/exit code only.
- `capture_pane` requests a bounded rendered scrollback window (`-S -N -E -`, optional `-J`), so it cannot guarantee complete stdout/stderr or recover history already evicted by tmux.
- `CommandSnapshot.output` and `output_truncated` are therefore presentation data, not a byte-preserving process-output channel; any replacement should preserve that distinction and the existing safety lease.
- Deployment inspection found the prior readiness commit is already pushed (`HEAD == origin/main`); current timeout/logging changes are uncommitted. Root `tmux-mcp.exe` and staged `target/release/tmux-mcp-rs.exe` both exist.
- Windows denied the broad `Win32_Process` inventory, so process restart targets must be discovered with narrower, read-only process/path queries before any stop action.
- Narrow process enumeration found two old `tmux-mcp` children (PIDs 42980 and 47036), both executing the repository-root binary; the host process is `codex.exe` (PID 12920) and must not be killed from this task.
- The first WMIC query used an invalid alias form and returned no process data; this is a command-shape error, not evidence that the processes vanished.
- Codex config confirms the exact MCP entry: `E:\\buyi_work\\tmux-mcp\\tmux-mcp.exe --ssh milab-eight --web-url http://127.0.0.1:38473 --client-name Codex`; this supplies restart arguments without changing any remote target.
- Replacement can remain recoverable by copying the root binary to a timestamped backup before placing the staged file.
- After stopping the children, no `tmux-mcp` process was automatically recreated within three seconds; the root binary now reports `tmux-mcp-rs 0.6.0` and matches the staged hash.
- `codex mcp list` from this shell reports no configured servers despite the inspected `%USERPROFILE%\\.codex\\config.toml` entry, so it is not a trustworthy restart mechanism for the already-running Codex host.
- The shell carries `CODEX_CI=1` and `CODEX_MANAGED_BY_NPM=1`; the CLI's “no MCP servers” result is likely a managed-host/config-scope difference, not proof that the active GUI/session has no tmux server.
- A disconnected manual `tmux-mcp.exe` launch would not be a valid stdio MCP restart, so it must not be used as a substitute for the host's lifecycle.
- The existing MCP tool registry remained visible locally, but its first read-only call failed with `Transport closed`; registry visibility is stale metadata, not a live server.
- Deployment is complete on disk: root binary hash matches the staged Release hash, old binary is recoverable at `tmux-mcp.exe.backup-20260902-002804`, and no `tmux-mcp` child remains running until the user restarts Codex.

## Web connection latency (2026-09-02)
- The UI connection badge is driven by `web/index.html`, not a WebSocket: the page repeatedly calls `/api/state` (750 ms) and pane capture (250 ms).
- The first symptom can therefore mean the initial `/api/state` request has not completed, rather than a transport handshake failure; exact server-side work still needs tracing.
- `refreshState()` has no client-side deadline. While one `/api/state` fetch is pending, `stateBusy` suppresses new requests and only records a queued refresh; the badge remains the initial “连接中” indefinitely.
- `/api/state` performs topology loading before returning any JSON. Topology loading is sequential: list sessions, server start time, then every session's windows and every window's panes.
- The likely latency amplifier is remote SSH process calls in that first topology load; if one SSH/tmux subprocess hangs, the page has no bounded request or intermediate-ready response.
- After the requested restart, the local listener is present on `127.0.0.1:38473` (PID 26484) and has an established local client connection; five root-path `tmux-mcp.exe` processes are visible, consistent with one web process plus stdio MCP processes. This makes a local TCP handshake failure less likely than a pending `/api/state` response.
- `cached_topology` holds its mutex while awaiting the entire topology load, so concurrent state requests queue behind the first remote scan instead of using the last cache entry.
- No remote command was issued during this audit; these conclusions are from local source and process/socket inspection only.
- `run_tmux_with_socket` (the path used by `list_sessions`, `list_windows`, `list_panes`, `server_start_time`, and `pane_info`) awaits `Command::output()` without an async deadline or `kill_on_drop`; the separate 10-second bounded helper is used for read-only filesystem/Git tools, not these tmux topology calls.
- The web process calls `tmux_version()` before binding the HTTP listener, so a fresh process can also delay the port during its first SSH-wrapped `tmux -V`; once the listener exists, the dominant wait is `/api/state` topology loading.
- The global tmux process semaphore is only 8 permits. It prevents an unbounded local subprocess stampede, but it does not make a call fast; a few stuck SSH/tmux children can consume permits and make capture or other requests wait behind them.
- SSH arguments are passed through as configured with no added `ConnectTimeout`, `BatchMode`, or command-level deadline. The remote list calls therefore inherit SSH's potentially long connection/authentication behavior.
- The topology scan performs one SSH-wrapped `tmux` process for sessions, one for server start time, then one per session's windows and one per window's panes. A selected pane adds another unbounded `display-message` call in the same `/api/state` request.
- The 250 ms capture poll is serialized by `captureBusy` and starts only after a pane is selected; it can still compete for the eight-process semaphore after initial state succeeds, but it is not the cause of the first “连接中” state.
- The current uncommitted logging change also makes `RecordStore::open` compact an oversized `events.jsonl` synchronously before the web listener binds. If the local log is over 4 MiB, this is a separate startup-only delay worth measuring; it does not explain a delay after an already-listening server receives `/api/state`.
- The actual local `%LOCALAPPDATA%\tmux-mcp\events.jsonl` is 4,167,258 bytes (just under the 4 MiB compaction threshold) as of this audit. It is large enough that synchronous startup parsing is nonzero, but it is not currently triggering the new compaction branch.
- `/api/state` also serializes operation/full-log records on every poll. The store is memory-backed after startup, so this is usually cheaper than SSH, but a near-4 MiB event log can still make the JSON payload and browser render noticeably heavy.
- `api_capture` uses the same unbounded tmux wrapper and performs a policy lookup before every 250 ms poll. Once interactive mode starts, a stalled capture can make the terminal feel slow independently of the state badge.
- Diagnosis: the page is reaching the local web server, but the first `/api/state` response is waiting on an unbounded, sequential SSH/tmux topology scan. The initial “连接中” text is never replaced until that response completes; this is a latency/timeout design issue, not a browser-side same-pane concurrency race.
- Smallest safe repair direction: bound and cancel the shared SSH/tmux subprocess path, then add a client-side request deadline so the UI reports “连接超时” and can retry. The durable performance fix is to refresh topology in the background or with one batched remote query; do not increase polling or blindly parallelize SSH calls.
- Read-only live check through the configured tmux MCP took roughly 45 seconds to return. It found 1 session, 11 windows, and 13 panes. The web topology path would issue about 14 sequential remote tmux/SSH calls for that shape (sessions + server start + windows + panes), which matches the reported “连接中/很卡” experience.
- This confirms the bottleneck is observable in the running environment, not only a theoretical code path. The check was read-only and did not execute shell text, send keys, or change remote state.
- The live `get-tmux-state` implementation in `src/server.rs` is also fully sequential: it lists sessions, then each session's windows, then each window's panes, and finally performs a separate current-session query. For the observed shape that is about 14 calls; the web loader is about 14 calls without a selected pane and 15 when it also fetches `pane_info`.
- For the observed 1/11/13 topology, a single `list-panes -a -F` style snapshot could carry session/window/pane fields in one remote invocation, while a controlled small concurrency pool is the safer fallback if batching proves incompatible with a tmux version.
- A local GET of `/` returned HTTP 200 in about 175 ms, so static page serving is not the bottleneck. I did not issue a second `/api/state` request because it would repeat the slow remote read.
- The live MCP state implementation confirms the same N+1 pattern and adds a final `get_current_session` call. For 1 session / 11 windows / 13 panes, it performs roughly 15 sequential SSH-wrapped tmux invocations.
- Batch design is viable with existing field encoding: `list-panes -a -F` can emit session, window, and pane identifiers/names/active flags in one response, then the existing policy filters and DTO assembly can rebuild the hierarchy locally. A second small query can retain server-start/current-session metadata where needed.
- Parallelism is a useful fallback, not the first choice: it lowers wall time to roughly the slowest window batch but creates up to 8 SSH processes per MCP process, can produce a mixed snapshot, and does not solve hangs without a shared timeout.
- No direct `futures` dependency or existing bounded-concurrency helper is present in the source; adding parallelism would either use Tokio's existing task primitives or introduce new plumbing. That makes the one-command batch path simpler and less invasive.

## Remote command latency optimization (2026-09-02)
- The tracked `execute-command` path does an extra `pane_info` lookup before wrapping the command, then sends the wrapped text and Enter as separate tmux/SSH calls when no delay is requested.
- `send-keys` with `delay_ms` deliberately sends one tmux call per character; this is expected for slow typing but is a design-induced cost when callers only need a normal text payload. Repeats and `enter` add further calls.
- Terminal finalization may issue up to three sequential `capture-pane` attempts; this is bounded for correctness but each attempt currently pays the unbounded SSH/tmux process path.
- The tracked launch path also performs a `pane_info` SSH lookup to choose the marker shell, then sends the wrapped command and Enter as two separate SSH/tmux calls. This is a real fixed overhead before `execute-command` can return.
- `get-command-result` without `waitMs` and the command resource path call `CommandTracker::check_status`; while a command is running that method performs a fresh `capture-pane` on every poll. A memory-only snapshot path removes this hidden remote round-trip while leaving explicit captures and wait-timeout captures intact.
- `delay_ms: Some(0)` currently takes the per-character branch in both tracker and direct `send-keys` code. Treating zero as no delay avoids one remote call per character without changing positive-delay behavior.
- tmux accepts multiple key arguments, so a no-delay non-literal payload plus `Enter` can be sent in one invocation; literal/chunked payloads must retain their existing separate handling.
- `CommandExecution` has no capture timestamp; avoiding hidden polling is smaller and safer than adding a per-command timestamp/cache map.
- Existing capture/retry tests call `check_status` directly, so the compatibility-preserving approach is to keep that method's explicit-refresh semantics and add a separate memory-only snapshot method for no-wait API/resource reads.
- The tmux adapter currently has one shared semaphore but no deadline around `run_tmux_with_socket`; `execute_command_bounded` already demonstrates the desired `kill_on_drop`/timeout pattern. Long-lived `wait-for` remains a deliberate exception and must not inherit the short command deadline.
- The new pure topology parser test passes. Stub-backed tests cannot run directly under native Windows because the repository fixture is a POSIX shell script; use the existing isolated proxy when validating process-path behavior.
- Web now has a 15-second fetch deadline and the interactive capture poll is relaxed from 250 ms to 500 ms; this makes a stalled request visible instead of leaving the badge at “连接中” indefinitely and halves avoidable capture pressure.
- The existing UI regression test asserted the old 250 ms hint; it must move with the intentional 500 ms polling change so the displayed guidance and test contract stay aligned.
- The full-feature compile confirms the new interactive `send-keys` branch and tracker snapshot API are wired without feature-gated errors.
- Direct `capture-pane` already bounds line ranges at the caller-specific level and now inherits the shared 15-second subprocess deadline; no additional output truncation policy was changed in this pass.
- `/api/state` still queried `pane_info` on every 750 ms state poll even when topology was cached. A one-entry, two-second web cache for the selected pane metadata removes that repeated design-induced SSH call without affecting policy authorization checks.
- Shared command deadlines also cover `send-keys`/dispatch. A timeout can make remote delivery uncertain, but these calls are expected to be short tmux injections; the existing tracker safety path remains unchanged for side-channel failures, and no timeout is applied to the long-lived `wait-for` watcher.
- The batch helper returns all records before policy projection. This preserves the existing output filtering, but should be checked against session/pane allowlist expectations before finalizing; if policy requires remote-side filtering, retain a scoped fallback for restricted configurations.
- The batch helper now tolerates an empty session list when the concurrent all-window/all-pane probes report the expected no-server/no-session error, preserving the old empty-topology behavior.
- The completed local design removes the repeated N+1 topology walk, so expected wall time is dominated by the slowest of three concurrent topology calls plus the current-session read rather than their sum. The single-digit-seconds estimate is an inference from the earlier ~45s sequential observation, not a new remote measurement.
- One lower-frequency MCP path, `resources/list`, still has its own session→window→pane loop. It is separate from the measured `get-tmux-state`/web path; if MCP client startup remains slow after deployment, reuse `list_topology` there as the next small follow-up.
- Cargo accepts one test-name filter per invocation; this is only a validation-command constraint and does not affect the implementation.
- A local `ssh -G` check showed OpenSSH keeps the first duplicate `ConnectTimeout`, so the initial default-option insertion would have blocked explicit user overrides. The helper now detects an existing timeout and only inserts the 5-second default when absent.
- The corrected timeout-argument path is now covered by a focused unit test and the full compile/lint/release checks.
- A truncated diff display caused only a local broken-pipe diagnostic from the output wrapper; it was not a source or runtime failure.
- The release artifact is newer than the root `tmux-mcp.exe`; keeping deployment separate ensures the currently running MCP process is not interrupted by this optimization turn.
- Literal/delayed `send-keys` behavior is partly intentional: delayed typing is one call per character, while ordinary literal payloads are already chunked to 4,000 UTF-8 bytes. The optimization target is avoidable validation/metadata calls and redundant polling, not removing safety limits.
- `get-command-result` with no `waitMs` calls `check_status`; while a command is running, `check_status` performs a fresh partial `capture-pane` on every poll. This can turn an agent's status loop into repeated SSH/capture traffic and is a stronger design-induced capture bottleneck than the final three-attempt retry.
- `wait_for` checks readiness via in-memory notifications and only invokes `check_status` at terminal/timeout boundaries, so its normal wait path does not need extra periodic captures.

## Timeout versus tracking error (2026-08-28)
- User reports that reaching a time budget is currently surfaced as `trackingerror` and should not be treated as an error.
- Required semantic split: a caller's `waitMs` expiry means “not ready yet”; elapsed background time must not end authoritative tracking; only protocol/process/capture failures should remain `tracking_error`.
- Current bounded-log work is unrelated and must remain intact while tracing status consumers.
- Sanitized local-log inspection confirms the dominant case: most `trackingerror` snapshots say `tracking deadline exceeded waiting for side channel` and arrive at roughly 607 seconds, matching the configured 600-second deadline plus final capture work.
- Genuine tracking failures also exist (`wait-for` connection reset and unreadable exit buffer), so renaming every `tracking_error` would destroy a useful distinction.
- The current control layer maps all `TrackingError` snapshots to failed actions, and `get-command-result` returns them as MCP tool errors; this is why routine long-running commands look like failures.
- Existing `waitMs` behavior is already correct: `wait_for_timeout_leaves_running` asserts that the command remains non-terminal, with `waitTimedOut=true` projected separately.
- `CommandStatus` currently combines three different outcomes in `TrackingError`: unreadable exit buffer, failed `wait-for`, and ordinary expiration of `tracking_deadline_seconds`.
- The watcher treats this combined status as uncertain: it retains the pane lease until a model/user-visible capture acknowledges it. Deadline expiry must preserve that safety behavior even if its public status becomes non-error.
- `get-command-result` marks `Failed` and `TrackingError` as MCP tool errors; a distinct timeout status can flow through its existing success branch without special response machinery.
- Serialization is inconsistent today: `#[serde(rename_all = "lowercase")]` emits `trackingerror`, while `CommandStatus::as_str()` emits `tracking_error`. A new timeout status should use an explicit stable wire name rather than repeat this mismatch.
- The Web command card displays the raw `commandSnapshot.status`; this is exactly where users see `trackingerror`. Its outer action record currently becomes `failed`, which also applies failure styling.
- The Web UI already supports the existing neutral `incomplete` action state. Mapping a command tracking timeout to `ActionStatus::Incomplete` avoids a second action-level enum and keeps old generic UI behavior.
- A new `CommandStatus::TimedOut` should remain terminal and “uncertain”: final bounded capture runs, the pane lease remains until the existing acknowledgement path, but `get-command-result` returns structured success rather than an MCP error.
- Further tracing shows a `TimedOut` terminal is still a poor fix: uncertain states trigger a global safety preflight that rejects the next MCP operation, while releasing the pane would allow a new command to collide with a possibly still-running old command.
- The root semantic error is treating `tracking_deadline_seconds` as a maximum command lifetime. A tracker should wait for the authoritative side-channel for as long as the command runs; only each caller's `waitMs` should expire.
- `send-cancel` already provides the bounded escape path for genuinely stuck commands, and killing a pane purges its tracker records. Therefore an arbitrary ten-minute terminal timeout is neither necessary nor safe.
- Best direction: remove the background deadline around `tmux wait-for`, keep real `wait-for`/exit-buffer failures as `tracking_error`, and retain a short independent bound only for final output capture.
- `wait_for_signal` currently awaits `Command::output()` without `kill_on_drop`; repeatedly timing out and restarting the wait would risk abandoned local tmux/SSH waiter processes and a signal-gap race. A renewal loop is therefore not a safe “small fix.”
- Waiting forever would also let a watcher survive pane purge without an existing per-command cancellation handle. Solving that correctly needs additional lifecycle plumbing, beyond a status-classification fix.
- The existing stub can deterministically make `wait-for` outlive a one-second tracker deadline, so the current `wait_for_timeout_leaves_running` test can be strengthened to distinguish caller timeout from background deadline without adding a new fixture.
- Implementation pins the original `tmux wait-for` future across periodic checks, avoiding both signal gaps and repeated child processes. If the command record is purged, dropping the future now kills its local tmux/SSH child via `kill_on_drop`.
- `CommandStatus` now serializes with snake_case; the only changed current value is `TrackingError`, and a serde alias preserves old `trackingerror` JSONL compatibility.
- The repository's Windows test failure is confirmed as fixture-only: `TmuxStub` writes extensionless POSIX scripts and joins PATH with `:`. Runtime code was not reached in the failed test.
- A target-local executable proxy can safely route only the fake `tmux` script through the installed Git `sh.exe`; it remains ignored and prevents any real tmux/SSH access during the regression.
- The checkpoint-crossing watcher regression passed through that fake-only proxy, proving the command remains non-terminal after the configured interval.
- Formatting, diff checks, 48 Windows-capable external tests, and Clippy with warnings denied passed. The full library run reached 194/206; its 12 failures are existing native-Windows fixture/path/concurrency assumptions, while both new regressions passed.
- The release build and version smoke test passed. The staged binary is `target/release/tmux-mcp-rs.exe`; the root installed executable was not replaced.


## Local agent-call logging (2026-08-24)
- Windows state resolves to `C:\Users\Mr.Buyi\AppData\Local\tmux-mcp` because `default_state_dir()` prefers `LOCALAPPDATA`.
- This directory contains mutable machine-local state (`events.jsonl`, gate state, AI pause state, and the control token), so a repository-local directory would be less safe and easier to commit accidentally.
- The existing Web Hub is the sole normal writer and serializes `RecordStore::upsert` calls through a Tokio mutex.
- `events.jsonl` currently has a 4 MiB compaction trigger, but retained records can still exceed that threshold; it is not a hard ceiling.
- Only command snapshots are explicitly trimmed toward the 1 MiB HTTP-body limit; generic tool results can exceed it and then fail to record.
- Current records contain enough metadata to diagnose tracked output readiness and subsequent `capture-pane` fallbacks; a second log stream is unnecessary.
- The live file was about 3.06 MB with 847 valid physical records / 830 logical IDs when inspected; large payloads were dominated by `read-file`, `execute-command`, and `capture-pane`.
- Supported read/search/git tools cap their returned data at 256 KiB, while the control API accepts at most 1 MiB per request; reducing records to 64 KiB would remove useful data without addressing the proven file-level gap.
- The minimal root fix is in `RecordStore::compact`: first apply the existing semantic retention sets, then retain only the newest contiguous suffix whose serialized JSONL bytes fit the 4 MiB budget.
- Keeping a contiguous newest suffix is preferable to filling leftover bytes with scattered older small records because diagnostics should preserve chronological context.
- The README did not explain the state directory, which made the standard AppData location look accidental; one local-state paragraph is sufficient documentation.
- Supported read/search/git tools cap their returned data at 256 KiB, while the control API accepts at most 1 MiB per request; reducing records to 64 KiB would remove useful data without addressing the proven file-level gap.
- The minimal root fix is in `RecordStore::compact`: first apply the existing semantic retention sets, then retain only the newest contiguous suffix whose serialized JSONL bytes fit the 4 MiB budget.
- Keeping a contiguous newest suffix is preferable to filling leftover bytes with scattered older small records because diagnostics should preserve chronological context.


## Requirements
- Diagnose why agents report needing `capture-pane` after commands finish.
- Diagnose why pane capture/output appears incomplete.
- Do not modify implementation.
- Do not affect remote state; if remote execution ever becomes necessary, first create a dedicated window and perform no modifying commands.

## Research Findings
- Previous inspection established that `execute-command` uses side-channel completion and stores output in `CommandTracker`.
- `capture-pane` is documented as a pane snapshot, not the authoritative tracked-command result.
- Command status and command output have different authorities: the private `wait-for`/buffer side channel establishes terminal status and exit code, while output is always reconstructed from pane scrollback using START/DONE markers.
- `check_status()` captures partial pane output only while status is `Running`; it returns immediately for a terminal record.
- `wait_for()` wakes when terminal status is committed and then calls `check_status()`, but that call cannot refresh output after terminal status. This creates a likely race if terminal status is published before the watcher finishes its final pane capture.
- Default output budgets are 1,000 lines while running and 16,000 lines for terminal marker bracketing. Losing START from bounded scrollback makes isolated output unavailable by design.
- The watcher explicitly commits terminal status, emits/notifies waiters, releases the pane, and only then runs final `capture-pane`. Therefore `get-command-result(waitMs=...)` can legally return terminal with `output=None`/`outputTruncated=true`; a later Updated event may fill the output.
- The shell signals the private side channel before it echoes the DONE marker. Final capture retries only three times with 10 ms gaps, so it can observe START without DONE and return an open/truncated output.
- Pane ownership is released before final capture. A next command can begin and advance scrollback while the prior command is still trying to recover its output.
- Complete output is reconstructed from the rendered terminal, not stdout/stderr. The parser trims surrounding whitespace and cannot recover text scrolled out, overwritten by carriage returns, cleared, or hidden in alternate-screen behavior.
- `tmux::capture_pane` requests `-S -N -E -` (and `-J`) but can only return history the pane actually retains. The repository never configures tmux `history-limit`; therefore `capture_max_lines=16000` is only a request ceiling, not a retention guarantee.
- Because complete extraction requires START and DONE in one bounded pane snapshot, any output that pushes START beyond tmux history causes `output=None`/truncated even though lifecycle tracking completed correctly.
- `execute-command(waitMs=...)` waits only for lifecycle and then discards the returned `CommandExecution`; its response schema contains command id/resource URI/status/message but never command output. A second model-visible operation is always required to read output, even when the command finished inside `waitMs`.
- The advertised preferred path is MCP resource subscribe/read, but ordinary exposed tool surfaces may give an agent only `get-command-result`; the execute response itself does not teach a one-call output path.
- Resource subscribers receive a Terminal notification before final capture, then possibly a second Updated notification after output capture. Reading on the first notification can produce a terminal result with missing/truncated output.
- `get-command-result(waitMs)` inherits the same race because `wait_for()` returns on terminal lifecycle, not on final-output readiness.
- The wire schema openly defines `output` as bounded pane text and `outputTruncated` as capture completeness, so full process output is not part of the current contract.
- Non-zero shell exit status is returned as an MCP tool error (`structured_error`) even though the structured snapshot may contain valid stderr/stdout. Clients or agents that treat tool errors as transport failures may ignore that payload and fall back to `capture-pane`.
- The existing resource tests generally assert terminal status/exit code, not that output is present and complete at the first terminal notification/read.
- Unit tests explicitly model terminal lifecycle and output readiness as two different waits: `wait_until_terminal()` is followed by `wait_until_output_refresh()`. This confirms the race is expected internally but is not represented in the public command status schema.
- Tests also codify that once START leaves scrollback, the tracker preserves an older partial output or returns no output, permanently marked truncated.
- The capture parser deliberately uses rendered marker brackets and `trim()`, so byte-exact or whitespace-exact command output is not supported.
- In the current Codex tool surface, tmux tools are exposed but no model-callable MCP `resources/subscribe` or `resources/read` operation is present. The `execute-command` description therefore recommends a preferred path the agent cannot actually invoke.
- `capture-pane` is prominently exposed beside `execute-command`/`get-command-result`; when a terminal result lacks output, it is the obvious available fallback even though it reads the same lossy pane history.
- Repository user documentation lists the tools but does not explain the two-stage terminal/output readiness behavior or warn that manual capture cannot recover lost history.
- Test coverage acknowledges marker loss and delayed output refresh using stubs, but the inspected tmux integration workflows primarily validate raw pane capture; there is no evident real-tmux large-output test proving tracked output completeness across history limits.
- Other code paths (`web.rs`/control) contain an explicit `presentation_ready = output.is_some() || !output_truncated` concept, suggesting the repository already recognizes that terminal lifecycle alone is insufficient for presentation readiness.
- The real-tmux audit integration test assumes `Completed` immediately implies output is present and asserts it in the same poll iteration. That assumption contradicts the watcher ordering and can be timing-dependent/flaky; it only exercises a one-line `echo`.
- The existing `presentation_ready` predicate means “some output exists, or capture is declared complete”; it accepts partial/truncated output as ready and cannot make missing-history output recoverable.
- If final capture produces the unchanged initial state (`output=None`, truncated=true), no Updated event is emitted, so consumers waiting for a post-terminal presentation update may never receive one.
- The server advertises MCP resource subscription capability and implements it, but capability advertisement does not make resource operations model-callable in the current Codex tool surface.
- Subscribing after a command is already terminal triggers an immediate notification based only on terminal lifecycle, again without checking final-output readiness.
- The primary server test named `execute_and_get_command_result_completed` asserts only status, exit code, and resource URI; it does not assert output presence/completeness. Thus the public regression suite would not catch the reported symptom.
- Pane resources themselves default to only 200 lines, making a resource/manual pane read typically less complete than the tracker's 16,000-line request.
- The `capture-pane` tool description correctly tells agents not to use it for routine command output and to prefer execute + get-result, so repeated fallback is evidence that the preferred path is not reliably satisfying the contract rather than simply missing guidance.
- Changelog history explicitly describes recovering command *tracking* when START scrolls out; this preserves lifecycle correctness but intentionally does not recover output completeness.
- The v0.6 changelog promises accurate reporting of incomplete/truncated output, not complete output capture; the current behavior matches that narrower claim.
- The same changelog says tracked executes have a per-pane queue, while current code rejects a second busy-pane command. This documentation/implementation drift is separate but reinforces that the agent contract needs auditing.
- Repository-local agent skills exist and may directly influence the observed fallback behavior; inspect `skills/tmux-via-mcp/SKILL.md` next.
- The repository skill labels the workflow “reliable output capture” and says `execute-command` provides “clean, attributable output,” which overstates the actual bounded/rendered-scrollback contract.
- The skill repeats the inaccessible resource subscribe/read preference and the stale queued-command claim. Agents following it will expect behavior the current Codex surface and implementation do not provide.
- The skill permits `capture-pane` for live progress and interactive flows, so once get-result returns terminal without ready output, the model has a documented escape hatch that naturally becomes the observed fallback.
- Large-output guidance points to buffer tools that are hidden from the default core surface, leaving default agents without the recommended scalable output path.
- The buffer-explorer skill operates only on pre-existing tmux buffers; `execute-command` does not stream stdout/stderr into such a buffer. It is therefore not an automatic recovery path for command output.
- Default exposure explicitly hides every buffer read/write/search tool unless `--full-tools` is enabled, confirming the standard Codex agent cannot follow the large-output recommendation.

## Ranked Diagnosis
1. **Confirmed API race:** terminal lifecycle is published before final output capture; waiters/readers can receive completed status with output still absent/truncated.
2. **Confirmed contract mismatch:** `execute-command(waitMs)` never returns output, and the preferred resource workflow is not model-callable in the current Codex surface.
3. **Fundamental completeness limit:** output is reconstructed from rendered, bounded tmux scrollback rather than captured stdout/stderr; history loss and terminal rendering are irreversible.
4. **Agent-guidance drift:** the skill promises reliable/clean output and queuing that current code/tool exposure does not provide.
5. **Coverage gap:** public/server tests do not assert output readiness at first terminal result; real-tmux coverage uses only a one-line command.

## Technical Decisions
| Decision | Rationale |
|----------|-----------|
| Trace shared output path before judging the symptom | A root-cause diagnosis must cover every caller and completion path |
| Treat status correctness separately from output completeness | The implementation deliberately makes the side channel authoritative only for lifecycle, not output bytes |
| Rank the terminal-before-output publication race as a confirmed contract problem | The code publishes terminal readiness before its result payload is actually ready |
| Treat the execute response shape as a usability/root-contract cause | `waitMs` suggests wait-for-result behavior but the tool cannot return the result payload |
| Treat manual `capture-pane` fallback as non-recovery | It reads the same bounded rendered scrollback and cannot restore lines already lost from history |
| Identify missing public “output ready” state as the core API mismatch | Tests wait for it separately, while users only see terminal lifecycle plus `outputTruncated` |
| Treat the inaccessible resource recommendation as an agent-guidance defect | Codex can see the recommendation but not the resource operations it names |
| Treat repository skill wording as a contributing cause | It promises reliable/clean output while the implementation only promises bounded presentation text |
| Preserve early internal terminal status but delay public readiness | Side-channel lifecycle remains authoritative without exposing a half-built result to waiters/subscribers |
| Hold the tracked-pane lease until final capture finishes | Prevents the next tracked command from racing the prior command's final scrollback snapshot |
| Do not build a stdout spool in this fix | It would solve the fundamental history ceiling but is a much larger transport redesign than the reported readiness race requires |
| Reuse readiness in control/web reconciliation | Their current `output.is_some() || !output_truncated` heuristic cannot distinguish partial output from a completed capture attempt |
| Write DONE before signaling the side channel | It removes the avoidable marker-order race while keeping exit status authority in the private buffer |

## Implementation Outcome
- `resultReady` (CommandSnapshot schema v2) now separates lifecycle completion from final bounded capture readiness.
- `wait_for`, resource Terminal notifications, control/web reconciliation, and tracked-pane release now use the same readiness boundary.
- `execute-command(waitMs)` returns a nested ready/timeout snapshot instead of discarding the waited result.
- Final capture is bounded by the existing tracking deadline; timeout yields a ready but truncated snapshot.
- The repository tmux skill now uses the callable tool path and accurately describes busy-pane rejection and bounded output.
- The fundamental tmux history/rendering ceiling remains: `outputTruncated=true` cannot be repaired by another `capture-pane`.

## Build Activation Findings (2026-08-24)
- The worktree still contains only the intended readiness fix across 10 tracked files plus the three planning files.
- The installed `tmux-mcp.exe` has not yet been replaced; all existing MCP processes therefore remain on the old image.
- The crate declares Rust 1.70 as its minimum supported version and has no repository-pinned toolchain file.
- Native Windows discovery found no `cargo`, `rustc`, `rustup`, or Docker executable; WSL, Scoop, and Winget are available.
- No staged `target/release` tmux binary exists in the repository yet.
- WSL itself is installed but has no Linux distribution, so it is not a usable build path.
- The repository's Windows release script expects the `x86_64-pc-windows-msvc` target and copies its release executable to `tmux-mcp.exe`.
- Rust's official Windows installation path is `rustup`; the detected Visual Studio C++ workload satisfies its native-linker prerequisite.
- Winget installed official Rustup 1.29.0 from `static.rust-lang.org` and verified the published SHA-256 before installation.
- The pre-existing Rustup settings select `stable-x86_64-pc-windows-msvc` with the default profile; Rustup honored that compatible configuration.
- The stable MSVC toolchain is installed and active, including Cargo, rustfmt, and Clippy proxies; no extra runtime service was introduced.
- Active compiler and package manager versions are Rust/Cargo 1.98.0 for `x86_64-pc-windows-msvc`.
- Initial `cargo fmt --check` found one mechanical line-wrap difference in `src/commands.rs`; no semantic issue was reported.
- `cargo test --all-targets` compiled the crate successfully but the library suite finished with 146 passed and 59 failed.
- Most failures are environmental: the Windows run cannot spawn the test suite's fake `tmux` command, and fake SSH-host tests reach the real Windows SSH client; three Unix-path policy assertions also disagree with Windows path semantics.
- At least one newly added readiness test (`final_capture_timeout_marks_result_ready_and_releases_pane`) is among the tests blocked by the same missing fake-tmux execution path, so the build must not be promoted until its logic is validated another way or the harness is made Windows-capable.
- Root cause of the broad unit-test failures is confirmed in `src/test_support.rs`: it writes POSIX `#!/bin/sh` scripts named `tmux`/`ssh`, marks them executable only on Unix, and hardcodes `:` as the PATH separator. Native Windows cannot execute those stubs.
- Git's `sh.exe` exists locally but is not on PATH; converting the entire harness is unrelated to the runtime fix, so validation should first use the Windows-capable external test targets and compiler/lint checks.
- All Windows-capable external targets passed: CLI 14, control 10, control-client 5, search 15, web 31, and web-UI 26 (101 total, zero failures).
- CI-style Clippy reached the project but Rust 1.98 raised one `result_large_err` warning-as-error at `src/web.rs:946`; this must be classified as pre-existing or fixed minimally before promotion.
- The new tracker regression coverage is concentrated in three stub-dependent readiness tests plus server response assertions; the POSIX fixture prevents those paths from executing natively on Windows.
- The Clippy warning was confirmed on an unchanged function signature. A function-local allow avoids an unrelated Axum response-type refactor; rustfmt and CI-style Clippy now pass.
- An ignored `target/`-local Windows proxy now routes test subprocesses into the repository's existing POSIX fake tmux/SSH scripts; it blocks real tmux/SSH and is not part of the source diff or release artifact.
- The new final-capture-timeout regression test passes through that isolated fake transport.
- The three side-channel completion/readiness tests also pass through the fake transport.
- `src/server.rs` belongs to the binary crate rather than the library crate; its response regression must be invoked with `cargo test --bin tmux-mcp-rs`, not `--lib`.
- The shared fake tmux now records the generated START marker ID from `send-keys`; the one-call `execute(waitMs)` server regression passes with `output="stub-output"` and `resultReady=true`.
- With the Windows proxy, the complete library suite improved from 146/205 to 194/205 passing. The 11 remaining failures are Windows path/SSH-output fixture issues plus one likely parallel environment collision; every newly added readiness test passed.
- The shell-marker failure remains when isolated and comes from the POSIX fake pane-info path on Windows; the separate wrapper-order test covering DONE-before-signal passes.
- The staged `x86_64-pc-windows-msvc` release build completed successfully in 47.8 seconds.
- The staged binary reports `tmux-mcp-rs 0.6.0`, renders CLI help successfully, and has a different SHA-256 from the August 2 installed binary.

## Commit Scope (2026-08-24)
- Commit 12 product files: readiness implementation, public schema/server/control/web integration, focused tests/test fixture, user-facing skill/changelog, and the tracked Windows binary.
- Exclude `task_plan.md`, `findings.md`, `progress.md`, and both old executable copies; they are local operational artifacts.
- `main` matched `origin/main` before staging, and the configured push remote uses HTTPS.

## Issues Encountered
| Issue | Resolution |
|-------|------------|
| 2026-09-02: exact test filter matched no test | Re-ran the parser filter without `--exact`; it passed |
| 2026-09-02: native Windows could not spawn the POSIX tmux stub | Kept validation local and use the previously isolated proxy for process-backed tests |
| 2026-09-02: PowerShell rejected an `rg src/*.rs` glob | Use explicit paths or `rg --files` instead |
| 2026-09-02: `rtk git diff` reported no repository even with `-C` | Inspect repository state with the PowerShell git invocation used by the workspace; no files were changed |
| 2026-09-02: Clippy rejected the inline pane-info cache tuple as `type_complexity` | Added a local type alias; full-feature Clippy then passed |
| Session catch-up script failed due to sandboxed `uv` cache access | Planning files did not already exist; initialized a fresh diagnostic plan |
| First PowerShell tool-discovery command lost `$` variables to the outer shell | Use a variable-free `Get-Command` query; do not repeat the same quoting form |
| WSL invocations failed because no distribution is installed | Do not retry or install WSL; check the native MSVC prerequisites and install only Rust if sufficient |

## Resources
- `src/commands.rs`
- `src/server.rs`
- `src/tmux.rs`
- `tests/integration.rs`
