DEVANA-FINDING: v1
Priority: P0 | Confidence: high | Security-sensitive: yes | Status: fixed
Location: src/security.rs:632-677, src/commands.rs:147-153, src/server.rs:1549-1554 | Slug: command-filter-shell-chain-bypass

# Command filter approves one string but the shell runs chained commands

## Finding

`SecurityPolicy::check_command` validates the user-supplied command text as a single regex match per line, but `CommandTracker::execute_command` injects that same text unquoted into a shell script (`echo "<START>"; <command>; echo "<DONE>"`) and sends it to the pane. Shell metacharacters such as `;` split the approved prefix from additional commands that never appear in the filter check.

## Violated Invariant Or Contract

When `security.command_filter` is enabled, every command that reaches a pane through `execute-command` should be blocked if any executed shell statement would violate the allow/deny patterns. The policy is documented as screening shell input for `execute-command`.

## Oracle

`README.md` denylist section (lines 534-537) states anchored patterns such as `^rm ` apply to `execute-command`. `security.rs` tests assert `rm -rf /` is denied with pattern `^rm `, implying the filter is meant to block `rm` execution, not only lines that start literally with `rm`.

## Counterexample

Config:

```toml
[security.command_filter]
mode = "denylist"
patterns = ["^rm "]
```

Call `execute-command` with `command: "true; rm -rf /"` on an allowed pane.

- `check_command` splits on newlines, finds one non-empty line `true; rm -rf /`.
- `^rm ` does not match the line start → policy allows it.
- Wrapped command sent to shell: `echo "TMUX_MCP_START_<uuid>"; true; rm -rf /; echo "TMUX_MCP_DONE_<uuid>_$?"`.
- The shell executes `rm -rf /` after `true`.

Allowlist variant: pattern `^git ` allows `git status; rm -rf /` for the same reason.

## Why It Might Matter

Operators who enable `command_filter` expecting a hard boundary can be bypassed by any allowed prefix followed by `;` and arbitrary shell statements. This is a direct policy bypass on the primary hardened input path.

## Proof

**Dataflow trace:** `ExecuteCommandInput.command` → `policy.check_command(&command)` (regex on full line) → `format!("...; {command}; ...")` (unquoted interpolation) → `tmux::send_keys` → shell parses `;` as statement separator → extra commands run.

**Counterexample value:** `"true; rm -rf /"` with denylist `["^rm "]`.

## Counterevidence Checked

- Multi-line newline bypass is tested and blocked (`security.rs` lines 832-845); same-line `;` chaining is not split or validated.
- `raw_mode` sends the command verbatim but still calls `check_command` on the same string, so the same bypass applies.
- README recommends `sh -lc '...'` for complex quoting but does not document semicolon chaining as an accepted limitation of denylist mode.

## Suggested Next Step

Validate commands in a shell-free way (argv-style execution) or parse shell statements and apply the filter to each statement; at minimum, reject `;`, `|`, `&`, `` ` ``, and `$()` in filtered mode unless explicitly allowed.

## Agent Handoff

After working this report, preserve the original finding body. Update line 2 `Status: ...` and the final `DEVANA-SUMMARY:` status. Use one of: `open`, `fixed`, `invalid`, `stale`, `duplicate`, `wontfix`. Add dated notes below with the evidence checked.

## Status Notes

- 2026-06-25: open by Devana. Initial report written from static source inspection.
- 2026-06-27: fixed. Working tree adds `collect_shell_statements` and per-statement checks in `check_command` with tests for `;`, `|`, `&`, `$(…)`, and backticks (`security.rs` lines 666–881, `test_command_filter_blocks_chained_statements`). Original unquoted `;` counterexample is blocked.

DEVANA-KEY: src/security.rs:632 | P0 | command-filter-shell-chain-bypass
DEVANA-SUMMARY: fixed | P0 high src/security.rs:666 - collect_shell_statements now splits chained statements before regex checks; uncommitted fix blocks the original `;` bypass.