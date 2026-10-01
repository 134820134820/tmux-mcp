# Findings: SSH efficiency

## Live SSH trace (milab-seven-intern, 2026-10-01)
- `ssh -vv … true`: TCP 250 ms; KEX/NEWKEYS ~2.6 s; auth accepted ~5.6 s; command ~0.5 s.
- 4 rounds default vs `KexAlgorithms=curve25519-sha256`: 3.5–5.7 s vs 3.9–7.6 s — no gain;
  jitter-dominated network. Only connection reuse helps.

## Pool E2E (read-only, test binary, no --web-url, 2026-10-02)
| call | installed v0.6.1 | new, pool off | new, pool on |
|---|---|---|---|
| capture-pane ×3 | 10.9 / 6.6 / 4.2 s | 3.9 / 4.5 / 6.7 s | 0.28 / 0.71 / 0.95 s |
| read-file /etc/hostname ×3 | 8.0 / 11.5 / 11.0 s | 8.4 / 8.4 / 11.0 s | 0.87 / 0.53 / 0.53 s |
| gpu-snapshot | 10.5 s | 12.9 s | 2.1 s |
| get-tmux-state (first, incl. connect) | 6.7 s | 8.2 s | 9.4 s |
- No orphaned pooled ssh after the MCP process was killed: the dispatcher exits on stdin EOF.

## Failure-mode safety review (2026-10-02, before install)
Question: can a network drop, overlapping requests, or an error make the new version send a
wrong command to a server?

| Path | Failure | Behaviour |
|---|---|---|
| Pooled request | connection cut mid-request | dispatcher acts only on a complete line; a partial line at EOF never runs (test `dispatcher_never_runs_a_partial_request_line`); SSH MAC prevents corruption |
| Pooled request | response lost / timeout | session killed, never reused; modifying requests never resent → existing "uncertain, stop and report" |
| Pooled read | reused session dead | resent once on a new session (reads only: capture/display/list/show + fixed read-only programs) |
| Pool → one-shot fallback | — | only when no session could start, i.e. nothing was sent; no double execution |
| Concurrent requests | — | each takes an exclusive session; response id checked; sessions keyed by ssh argv (admin/intern never share) |
| Typed tracked command | — | text + Enter in one tmux call: all or nothing |
| paste-text | truncated stdin upload | staged buffer compared byte-for-byte with `show-buffer` before `paste-buffer`; mismatch deletes it, nothing typed (pre-existing risk, now closed) |
| write-file | truncated stdin | size checked against the sent length before rename; target untouched |
| script upload | truncated stdin | size checked before keeping; a truncated script (could still pass `bash -n`) is deleted, nothing typed |
| script launch | — | only `bash '<validated /abs/path/<uuid>.sh>'` is typed; newline in path rejected by tracker |
| watcher polling | network loss | read-only polling; pane released only with exit buffer + DONE marker |
| write-file overwrite | shared dir | REMOVED 2026-10-02 by user decision: any regular file in a writable dir can be replaced |
| script cache prune | symlinked dir | only own uuid-named files in an owned real directory |

## External review (Codex, 2026-10-02 01:17–01:21) — A, B, C fixed in source; D won't fix
User decision: no write-permission limits at all (D dropped; ownership rule removed too). A fixed
by a `./` prefix on relative paths (+ `ln --`), no path rejected. B: polling accepts exit buffer
with Complete OR MissingStart (Open still waits). C: uncertain vs not-written classification.
Source: `.planning/2026-10-02-review/` (review only; its harness is in ignored `target/review/`).
Written by a Codex session that read the working tree during our install; missed by our own
safety review. All four re-verified against source on 2026-10-02 after compaction.

| # | Where | Defect | Proposed fix |
|---|---|---|---|
| A | tmux.rs `WRITE_FILE_SCRIPT` `ln "$tmp" "$p"` | no `--`: a path like `-tother` is parsed as `ln -t other`, so the file lands in `other/.tmux-mcp-write.*` and the tool reports success. Matches the user's "error → wrong action" concern | `ln -- "$tmp" "$p"`; also reject paths starting with `-` in `write_file` (other commands already use `--`) |
| B | commands.rs watcher polling (~l.980) | after a `wait-for` drop, completion needs exit buffer AND a Complete START/DONE capture; if START scrolled out of `capture_max_lines`, the command stays Running and the pane stays leased forever (fails closed, but stuck). Check predates this work but is now the main path for drops | in `watcher_lost` mode treat a readable exit buffer alone as completion — the same evidence the signal path (`Ok(())` → `read_exit_code_after_signal`) already accepts; final capture handles incomplete output |
| C | server.rs `write_file` error → "File not written: …" | a transport/protocol error after the remote rename is reported as not written, though the file may exist | transport/timeout errors → "write result uncertain; check with read-file/file-stat before retrying"; keep "not written" only for script exit codes 2–11 |
| D | security.rs `check_tool` | `write-file` only needs `allow_execute_command`; a command allow-filter (e.g. only `^ls$`) does not restrict it, while `script` fails closed under a filter | fail closed: refuse write-file whenever a command filter/pattern list is configured |

Residual risks (pre-existing, not introduced):
- Bracketed paste leaves text on the input line; a later execute-command on that pane appends to
  it (seen in E2E). Prefer write-file/script. Candidate fix: mark a pane "pending input" after
  paste-text/send-keys without Enter and refuse execute-command until Enter/Ctrl-C.
- Separate MCP processes (two Claude sessions) do not share pane occupancy.

## Resource URIs not target-qualified (found 2026-10-02 during test repair; NOT fixed)
- `read_resource`/`subscribe` require `tmux://<target>/…`, and templates use `{target}`, but
  `list_resources` still emits `tmux://server/info`, `tmux://pane/%1`, … and the clients template
  is `tmux://clients` — none of these can be read back (they parse as target "pane"/"server",
  or fail). Agents mostly use tools, so impact is low; fix = qualify with `targets::uri`, or
  drop the listing. User decision.

## Environment hazards
- Git-for-Windows `ssh` (MSYS) first on PATH makes every MCP ssh call hang until timeout, for
  the installed binary too. Clients launched natively resolve `C:\Windows\System32\OpenSSH`.
- (fixed 2026-10-02) Stale test stub: `display-message` matched literal `#{pane_current_path}`
  while `pane_info` uses `#{s,%,%25,…:pane_current_path}`; now matches `*pane_current_path*`.
- One unexplained 30 s pooled connect stall right after a fresh test build; not reproduced in
  5 repeats. A connect failure sends nothing remotely and is reported as a transport error.
