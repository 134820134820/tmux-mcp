DEVANA-FINDING: v1
DEVANA-STATE: invalid | P0 | high | security=yes
DEVANA-KEY: src/security.rs:790 | command-filter-quote-breakout-bypass

# Command filter misses shell statements after a closing double quote in wrapped execution

## Finding

`collect_shell_statements` keeps semicolons inside double-quoted regions, so a payload such as `x"; rm -rf /` is validated as one statement. `CommandTracker::execute_command` then interpolates that text unquoted into `echo "<START>"; <command>; echo "<DONE>"`, where bash closes the synthetic quote after `x` and runs `rm -rf /` as a separate command that never appears in the filtered statement list.

## Violated Invariant Or Contract

When `security.command_filter` is enabled, every shell statement reachable from user input through `execute-command` (and other `check_command` gates) must be screened. `check_command` documents that unquoted interpolation into a shell line requires per-statement validation.

## Oracle

`security.rs` tests block unquoted `;` chaining (`test_command_filter_blocks_chained_statements`) and document unquoted wrapping in `check_command` (lines 658–665). `commands.rs` line 152 wraps commands without additional quoting.

## Counterexample

Denylist pattern `^rm `, user command `x"; rm -rf /`, allowlisted pane. `check_command` approves the single collected statement. Wrapped shell line `echo "TMUX_MCP_START_…"; x"; rm -rf /; echo "TMUX_MCP_DONE_…_$?"` executes `rm -rf /` after the empty quoted segment following `x`.

## Why It Might Matter

Operators relying on command deny/allow lists for agent containment can be bypassed without `raw_mode` or `literal` send-keys, enabling arbitrary command execution in an allowed pane.

## Proof

Dataflow trace: MCP `execute-command` input → `server.rs` `check_command` → `collect_shell_statements` (one statement) → `commands.rs` wrap → tmux `send-keys` → shell parses quote breakout → `rm` runs outside filtered text.

## Counterevidence Checked

Single-quoted payloads are intentionally not split (`test_command_filter_respects_single_quotes`). Escaped `\;` is kept literal in the parser and does not reproduce this breakout. `raw_mode` skips wrapping but still calls `check_command` first; this path uses normal tracked execution.

## Suggested Next Step

Add an integration test with denylist `^rm ` and command `x"; rm -rf /`, then fix by validating the fully wrapped shell line or rejecting metacharacters/quotes that change parsing in context.

## Status Notes

- 2026-06-27: open by Devana. Initial report written from static source inspection.
- 2026-07-05: invalid after triage. `CommandTracker::execute_command` does wrap as `echo "<START>"; {command}; echo "<DONE>"`, but the reported payload `x"; rm -rf /` leaves bash with an unmatched double quote in the resulting command line. Harmless equivalent validation with `/bin/bash -c 'echo "START"; x"; echo BAD; echo "DONE_$?"'` failed at parse time with `unexpected EOF while looking for matching '"'` and did not execute `BAD`.

DEVANA-KEY: src/security.rs:790 | command-filter-quote-breakout-bypass
DEVANA-SUMMARY: invalid | P0 | high | Original quote-breakout counterexample does not execute the trailing denied command; bash rejects the wrapped line as an unterminated double quote before `BAD` or equivalent trailing commands run.
