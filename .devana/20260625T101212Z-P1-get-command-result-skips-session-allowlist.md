DEVANA-FINDING: v1
Priority: P1 | Confidence: high | Security-sensitive: yes | Status: fixed
Location: src/server.rs:1537-1541,1619-1633,3253-3277 | Slug: get-command-result-skips-session-allowlist

# get-command-result does not enforce allowed_sessions on poll

## Finding

`execute-command` calls both `check_pane` and `enforce_session_for_pane` before tracking a command. `get-command-result` and the `tmux://command/{id}/result` resource only call `check_pane` (and socket checks). They never re-resolve the pane's session or call `enforce_session_for_pane`, so polling can read output from panes in sessions that are no longer allowlisted.

## Violated Invariant Or Contract

Session allowlist enforcement should be consistent across write and read paths targeting the same pane. README states `allowed_sessions` limits operations scoped to session/window/pane.

## Oracle

`capture-pane` and `read_resource` for `tmux://pane/{id}` call `enforce_session_for_pane` (server.rs lines 1007-1008, 3019-3025). `pane_tools_deny_unlisted_sessions` tests expect session enforcement on capture. `get_command_result_denied_for_pane` only covers `allowed_panes`, not sessions.

## Counterexample

1. `allowed_sessions = ["%1"]`, pane `%2` in session `%1`.
2. `execute-command` on `%2` with `sleep 300` → allowed → returns `command_id`.
3. `join-pane` or `break-pane` moves `%2` into session `%3` (not allowlisted).
4. `get-command-result` with `command_id` → passes `check_pane` only → `check_status` captures pane output from session `%3`.
5. `capture-pane` on `%2` after the move would call `enforce_session_for_pane` and deny access.

## Why It Might Matter

Agents routinely poll command results after layout changes. This leaves a read path open on disallowed sessions while equivalent capture tools are blocked.

## Proof

**Cross-entry mismatch:** `execute_command` (1537-1541) vs `get_command_result` (1619-1623) vs `read_resource` command branch (3261-3267).

**Dataflow trace:** `check_status` → `tmux::capture_pane(&execution.pane_id, ...)` with no session gate on poll.

## Counterevidence Checked

- At execute time, session was valid; the defect appears after topology changes or if tracker entries are created without server guards (tests inject via `tracker.execute_command` directly).
- Socket binding checks (1597-1616) are separate from session allowlist and do not compensate.
- `list_resources` command URIs also skip session checks (lines 2827-2828).

## Suggested Next Step

Call `enforce_session_for_pane(&cmd.pane_id, socket.as_deref())` in `get_command_result` and the command `read_resource` branch before `check_status`, matching `execute-command` and pane resources.

## Agent Handoff

After working this report, preserve the original finding body. Update line 2 `Status: ...` and the final `DEVANA-SUMMARY:` status. Use one of: `open`, `fixed`, `invalid`, `stale`, `duplicate`, `wontfix`. Add dated notes below with the evidence checked.

## Status Notes

- 2026-07-05: repair after validator RED. `get-command-result` and `tmux://command/{id}/result` now use a fresh pane info lookup for the tracked pane session before `check_status`, bypassing the 5-second session cache only for command-result read paths. Added cache-primed regressions that call real `execute_command` with pane session `%1`, then change the pane to session `%2` before polling/reading command output. Evidence: `cargo test --bin tmux-mcp-rs get_command_result` and `cargo test --bin tmux-mcp-rs read_resource_command` passed.
- 2026-07-05: fixed by adding `enforce_session_for_pane` to both `get-command-result` and the `tmux://command/{id}/result` resource path before polling/capturing tracked command output. Added focused regressions for disallowed current pane sessions on both paths. Evidence: `cargo test --bin tmux-mcp-rs unlisted_session`, `cargo test --bin tmux-mcp-rs get_command_result`, and `cargo test --bin tmux-mcp-rs read_resource_command` passed. The suggested `cargo test --lib server::tests::get_command_result` was also run and compiled successfully but matched 0 tests because these server tests are under the binary target.
- 2026-06-25: open by Devana. Initial report written from static source inspection.

DEVANA-KEY: src/server.rs:1619 | P1 | get-command-result-skips-session-allowlist
DEVANA-SUMMARY: Status=fixed | P1 high src/server.rs:1619 - get-command-result and command result resources now enforce the tracked pane's fresh current session before polling tracked command output, blocking stale cached allowed sessions after pane moves.
