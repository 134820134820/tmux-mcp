# Task Plan: SSH efficiency, transport resilience, write-file, scripts

## Goal
Make tmux-mcp faster and safer from the user's action-log evidence: reuse SSH connections, stop
transport drops from pausing the AI, and replace paste-text heredoc workarounds with tracked,
safe primitives. Top user priority (2026-10-02): no accident — network drop, overlapping
requests, or an error — may ever send a wrong command to a server.

## Current Phase
Phases 0–12 INSTALLED 2026-10-02 03:27 (root `tmux-mcp.exe`). Changes staged; commit and the
exe history purge are run by the user (hook blocks commits on main). See Phase 12.

## Next Step
Report status / wait for the user. Open decisions (ask, do not assume):
0. Phase 10 (review fixes A/B/C + ownership rule removed) is in source and tested in
   target/test-build; NOT installed. Installing needs user OK to stop running MCPs again.
1. Implement "pending input" guard? After paste-text or send-keys without Enter, mark the pane;
   refuse execute-command on it until Enter/Ctrl-C (residual risk seen in E2E: pasted text was
   concatenated with the next typed command).
2. (decided 2026-10-02) No write-permission limits: ownership rule removed, D not done.
3. Add Windows hardening: prefer `C:\Windows\System32\OpenSSH\ssh.exe` over PATH lookup?
4. Commit? Uncommitted tree mixes the previous task (tracking pause UI, 2026-09-22 plans) with
   this task. Never commit without the user asking; no attribution lines in commits.
User-side actions still pending: Claude Code sessions run `/mcp` → Reconnect; the Codex session
started at 01:16 (PID 141436) still runs the OLD binary until Codex restarts.

## Phases

### Phase 0 (earlier in session): Web UI pause panel — Status: complete
- `web/index.html`: tracking-pause notices + global AI pause merged into one collapsible
  `#safety-panel` (tokens --red-soft/--amber-soft/--green-soft…), inline confirm instead of
  `confirm()`, handled pauses folded into "已处理 N 条", chevron left when collapsed / down when
  open. Tests: `tests/web_target_switch.mjs` (node), `tests/web_ui.rs`.

### Phase 1: Watcher resilience — Status: complete
- `src/commands.rs` `spawn_side_channel_watcher`: on `wait-for` transport error keep tracking by
  polling the durable exit buffer + DONE marker (2→30 s backoff); vanished pane/server
  (`tmux::is_missing_tmux_target`) → TrackingError "pane or tmux server disappeared".
  `read_exit_code_after_signal` retries transport errors (`tmux::is_transport_error`).

### Phase 2: Persistent SSH sessions — Status: complete
- `src/ssh_pool.rs`: per ssh argv (test builds add thread id), ≤ 4 idle sessions, 600 s idle
  expiry, bash dispatcher (base64 line protocol, nonce banner, temp-file stdio), ping before a
  modifying request on a session idle > 20 s, reads retried once on a lost reused session,
  unsupported host → one-shot fallback for 600 s, `TMUX_MCP_SSH_POOL=0` disables.
- `src/tmux.rs`: pooled paths in `run_tmux_with_socket_inner`, `execute_pooled_bounded`
  (read-only programs, git), `run_shell_snippet` (modifying, never resent). One-shot budgets get
  `SSH_SETUP_ALLOWANCE` (20 s). Timing transports `ssh-pool` / `ssh-pool-new`.

### Phase 3: write-file tool — Status: complete
- Temp file in target dir → `ln` (create, never clobbers) or `mv -f` (overwrite, keeps mode);
  refuses symlinks/non-regular files; ≤ 512 KiB; gated by `allow_execute_command` + Gate.

### Phase 4: execute-command `script` — Status: complete
- Exactly one of command/script; no detach/delayMs; ≤ 64 KiB; CRLF→LF; NUL/lone CR rejected;
  `policy.check_command(script)`; busy pane rejected before upload; upload to
  `~/.cache/tmux-mcp/scripts/<uuid>.sh` (umask 077) + `bash -n`; pane receives only
  `bash '<path>'`; scripts kept 7 days (prune: own uuid-named files only).

### Phase 5: notify — Status: complete
- `notify: true` (needs `--claude-channel`) → one channel message per finished command;
  `call_tool` now captures the peer (channel messages were silently dropped before).

### Phase 6: press-special-key aliases — Status: complete

### Phase 7: Docs, build, verification — Status: complete
- README, docs/TOOL_SURFACE.md (22 / 25 tools), docs/IMPROVEMENTS.md (2026-10-01 section),
  scripts/summarize-timings.ps1 (pooledRequests/newSessions) + self-test.

### Phase 8: Failure-mode safety review — Status: complete
- Truncation guards: write-file & script upload check received size vs sent; paste-text
  compares staged buffer byte-for-byte before `paste-buffer`. Dispatcher runs only complete
  request lines (tested). Overwrite only own+writable files. Full table: findings.md.

### Phase 9: Install — Status: complete
- Backup `target/deployment-backups/tmux-mcp-before-20261002.exe`; Codex-held old exe moved to
  `target/deployment-backups/tmux-mcp-in-use-by-codex-20261002.exe`; new exe from
  `scripts/build-release.ps1` output; web hub restarted in a new pwsh window (HTTP 200, pooled).

### Phase 10: Review fixes (2026-10-02, after compaction) — Status: complete (not installed)
- User: "涉及到写文件权限的事情，目前不要限制…正常写就完事了". Removed the owner/writable
  overwrite rule; D (allowlist restricts write-file) not done.
- A: `WRITE_FILE_SCRIPT` prefixes relative paths with `./`, `ln --`. B: watcher polling accepts
  exit buffer + (Complete | MissingStart). C: `tmux::write_file` returns "File not written: …"
  only for script exit 2–11 with empty stdout or a local queue timeout; anything else
  "Write result uncertain …"; server prints tmux messages as-is.
- Tests: `write_file_reports_a_lost_reply_as_uncertain_not_unwritten` (ssh stub
  `TMUX_STUB_SSH_DROP_REPLY`), `watcher_polling_finishes_when_start_marker_scrolled_out`
  (fails without fix), dash names in the write-file test. tools/e2e_write.mjs updated.

### Phase 11: Test suite repair (2026-10-02) — Status: complete (not installed; tests only)
- User: fix what should be fixed, delete what should be deleted; keep tests lean and critical;
  no trivially-passing tests added for show.
- Stub `display-message` matches `*pane_current_path*` → fixed 12 (split/break, session
  policy, pane-shell markers). Resource policy tests → `read_resource_inner` + `{target}`
  templates; one routing test that an unqualified URI is rejected. Updated stale assertions
  (uncertain delivery text, socket "not allowed", no command echo). final-capture timeout test:
  5 s stub capture, 10 s wait (verified failing without the bound). canonicalize test
  `#[cfg(unix)]`. Deleted: 5 trivial resource tests, misnamed get_command_result_tmux_error,
  bootstrap string-shape test, special-key alias enumeration; brittle capture-count assert;
  channel-content test trimmed to the tracking-error case.
- Result: lib 238/238, bin 385/385, cli/web/web_ui 75; clippy (default + all features) clean.
- Found (not fixed, user decision): resource listing emits URIs without target (findings.md).

### Phase 12: Final batch (2026-10-02) — Status: installed; commit + purge await the user
User decisions: ABC stay, install once at the end with everything. Then:
- [x] Paste guard. Log study (tools/paste_sequences.mjs): 46 paste-text, each a complete
      command; 2 back-to-back pastes were separate files; some panes submit a trailing
      newline, others keep it. → refuse execute-command / second paste-text on an idle pane
      after Enter-less input until Enter or Ctrl-C (server `unsubmitted_input`).
- [x] Windows: `tmux::ssh_program()` → TMUX_MCP_SSH_PROGRAM, else System32 OpenSSH, else PATH.
      Verified with Git ssh first on PATH (E2E_GIT_SSH_FIRST=1).
- [x] Resource listing: per-target server/info + clients + tracked commands, all qualified;
      no topology enumeration; clients template `tmux://{target}/clients`.
- [x] Tests: lib 238, bin 377 (×2 full runs), cli/web/web_ui 75, node UI test; clippy default,
      all-features, no-default-features clean. Real server: e2e_write 21/21, e2e_drop 10/10.
- [x] Installed 03:27: stopped PIDs 141436 (Codex MCP) + 139696 (hub); backup
      target/deployment-backups/tmux-mcp-before-final-20261002.exe; hub restarted (HTTP 200,
      ssh = System32 OpenSSH).
- [ ] Commit: all changes STAGED; hook blocks commits on main → user runs
      `git commit -F <scratchpad>/commit_msg.txt`.
- [ ] Purge old exe: user runs `bash <scratchpad>/purge_old_exe.sh` (bundle backup, local
      filter-branch on main, re-commit current exe, gc). Push needs --force-with-lease and a
      decision about remote branch codex/tmux-web-console (also holds old exes).

## Key Questions (answered)
- Why slow? Per-call SSH handshake (~5–6 s of ~6.9 s), not remote work → connection reuse.
- Why AI pauses? 40/44 were the watcher's own SSH connection resets → poll instead.

## Decisions Made
| Decision | Rationale |
|----------|-----------|
| Pool instead of merging SSH calls per tool | Each extra call costs ~1 RTT once pooled |
| Script runs in child bash | `exit`/`set -e` cannot kill the pane shell; cd/export documented as not persisting |
| Scripts kept 7 days, not deleted | Human can inspect what actually ran |
| `script` is an execute-command field, not a new tool | Reuses Gate/policy/UI paths keyed on execute-command |
| write-file under `allow_execute_command` | Writing files is as powerful as running commands |
| notify errors without `--claude-channel` | Never silently drop a requested notification |
| Kill only MCP processes, never claude.exe/codex.exe; rename an in-use exe | Avoid interrupting active agent sessions |

## Errors Encountered
| Error | Attempt | Resolution |
|-------|---------|------------|
| 60 lib tests "program not found" on Windows | 1 | `tmux::external()` runs stubs via `sh` in test builds; PATH joined with platform separator |
| `MSYS=noglob` | 1 | Rejected: also disables quote removal |
| Tests hung: pooled sessions shared across tokio test runtimes | 1 | Test-only pool key adds thread id; TmuxStub clears pool and defaults `TMUX_MCP_SSH_POOL=0` |
| Bin tests spun forever (call_tool without target) | 2 | cfg(test) fallback target `test-target`; explicit target in tool_first_gate test |
| `Command::new("bash")` hung on Windows | 1 | System32 bash.exe is the WSL launcher; tests use `sh` |
| Dispatcher test: SHELL=sh rewritten to cwd path by MSYS | 1 | Use `/bin/sh` |
| E2E harness: every ssh hung (also old exe) | 1 | Git's MSYS ssh first on PATH; harness puts System32\OpenSSH first |
| Notify never arrived in E2E | 1 | Peer only captured on resource requests → capture in `call_tool` |
| E2E paste check expected execution | 1 | Bracketed paste waits for Enter; test presses Enter |
| `build-release.ps1` copy failed: exe in use | 1 | New Codex MCP; moved in-use exe aside, copied build |
| Hook blocks chained shell commands (`;`, `&&`, heredoc, until-loop) | n | Use single commands, script files, or Edit/Write tools |
