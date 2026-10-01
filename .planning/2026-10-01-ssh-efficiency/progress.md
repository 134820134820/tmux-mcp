# Progress: SSH efficiency

## Resume here (written 2026-10-02 before context compaction)
- State: everything implemented, verified, and INSTALLED. Next = user's open decisions in
  task_plan.md "Next Step". Do not start new work without the user.
- User language: Chinese. User priority: no accidental wrong command on servers.
- Repo: E:\buyi_work\tmux-mcp (Windows; Bash tool = Git bash; pwsh 7 for PowerShell).
- Shell hook blocks chained commands (`;`, `&&`, `|` chains with loops, heredocs): run one
  command per call or put steps in a script file.

## Tools (in ./tools, copied from the session scratchpad)
- `node tools/e2e_readonly.mjs <exe> E:/buyi_work/tmux-mcp/targets.toml <target> <pool 1|0>`
  — read-only timing (get-tmux-state, capture ×3, read-file /etc/hostname ×3, gpu-snapshot).
- `node tools/e2e_write.mjs <exe> E:/buyi_work/tmux-mcp/targets.toml milab-seven-intern`
  — MODIFIES the host: throwaway session `tmux-mcp-e2e`, /tmp/tmux-mcp-e2e, script cache;
  cleans up afterwards. Run only with user approval. Last run: 19/19 pass (Phase 10 build).
- `node tools/e2e_drop.mjs <exe> E:/buyi_work/tmux-mcp/targets.toml milab-seven-intern`
  — throwaway session `tmux-mcp-e2e-drop`; kills the test MCP's own watcher ssh to simulate a
  network drop. Last run: 10/10.
- `pwsh -NoProfile -File tools/analyze.ps1` / `analyze2.ps1` — action-log analysis
  (%LOCALAPPDATA%\tmux-mcp\events.jsonl). `tools/mcp_detail.ps1` — running MCP processes.
- Both node harnesses put `C:\Windows\System32\OpenSSH` first on PATH (Git ssh hangs) and set
  LOCALAPPDATA to a temp dir; no `--web-url`, so the live hub is untouched.

## Verification commands
- `cargo test --lib` (~40 s) → expect 238 pass, 0 failures (since Phase 11).
- Bin tests: `cargo test --bin tmux-mcp-rs --no-run`, then run the exe it prints (was
  `target/debug/deps/tmux_mcp_rs-780fd9ed841ac8a3.exe`; the hash changes on rebuild)
  with `--test-threads=1` (~80 s run directly) → expect 385 pass, 0 failures (since Phase 11).
- `cargo test --test cli --test web --test web_ui` → 14 / 34 / 27 pass.
- `node tests/web_target_switch.mjs`; `pwsh -NoProfile -File scripts/test-summarize-timings.ps1`.
- `cargo clippy --all-targets` clean; `cargo fmt` applied.
- No known failures since Phase 11 (see task_plan.md Phase 11 for what was fixed/deleted).

## Session log
### 2026-10-01
- Web UI: safety panel redesign + chevron fix; UI-only build installed earlier that day.
- Analysis from action log + live `ssh -vv` trace (task_plan.md Key Questions, findings.md).
- Test harness made Windows-capable (60 → 10 lib failures).
- Phases 1–7 implemented with tests; read-only real-server E2E: capture/read 0.3–1.1 s vs
  4–11.5 s, gpu 0.7–2.1 s vs 10.5 s.

### 2026-10-02
- User approved modifying E2E; found and fixed peer capture (notify never fired).
- Safety review (findings.md table): truncation guards (write-file, script upload, paste-text),
  overwrite ownership, prune scope, partial-request atomicity test.
- Installed: user-approved stop of PIDs 131108 (web hub), 91264, 134904 (Claude Code MCPs);
  Codex MCP 141436 (started 01:16, after the stop) left running on the old exe (moved aside).
  Web hub restarted in a new pwsh window: `.\tmux-mcp.exe --web --web-bind 127.0.0.1:38473
  --targets .\targets.toml` (HTTP 200, pooled sessions active).
- Backups: target/deployment-backups/tmux-mcp-before-20261002.exe,
  tmux-mcp-in-use-by-codex-20261002.exe; older target/tmux-mcp.replaced-20261001.exe (pre-UI).
- Post-compaction review of these files: found `.planning/2026-10-02-review/` (Codex review,
  4 confirmed defects, not fixed) → findings.md "External review", task_plan.md Next Step 0.
  Fixed stale docs/IMPROVEMENTS.md header ("not yet installed") and lib test count.
- Phase 10 (user: no write-permission limits; fix the rest): A/B/C fixed, ownership rule
  removed, D dropped. Results (target/test-build): lib 230 pass / 10 known; bin 356 pass / 38
  known (same counts + 2 new tests); cli/web/web_ui 75 pass; clippy clean; fmt applied. The new
  watcher test fails with the fix reverted. NOT installed.
- Real-server E2E with target/test-build/release/tmux-mcp-rs.exe on milab-seven-intern (user
  approved): e2e_write 19/19; new tools/e2e_drop.mjs 10/10 (kills only the test MCP's own
  `wait-for` ssh child; short output finished 9.9 s after drop with output intact; 5000-line
  output with START scrolled out finished 8.9 s after drop; pane reusable). Sessions removed.
- 38 bin failures classified (answer to user): 20 resource tests use pre-v0.6.1 URIs without
  target ("unknown target: pane"); 8 split/break-pane use stale tab-separated stub output;
  6 assert pre-09-22 messages/fields; 4 Windows-only (canonicalize path form, MSYS-mangled
  pane-shell format, 2 capture-timing). Lib's 10 are the tmux/commands subset. Recommended:
  update tests (they cover policy checks), not delete; Windows-only → fix or cfg(windows) ignore.
- Phase 11 done (user: fix/delete, keep lean): all suites green — lib 238, bin 385,
  cli/web/web_ui 75; clippy clean. Timing tests repeated 3× in parallel: stable. Not installed
  (test-only changes; Phase 10 product changes still pending install).
- Phase 12 (paste guard, System32 ssh, resource list) implemented and verified; bin now 377
  (obsolete list tests removed). dispatcher partial-line test flaked twice at 10 s on Windows
  MSYS under the full run → 30 s step; two clean full runs after. Installed 03:27; all
  changes staged; commit + exe history purge handed to the user (main is hook-protected).
