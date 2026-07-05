DEVANA-FINDING: v1
DEVANA-STATE: fixed | P1 | high | security=yes
DEVANA-KEY: src/security.rs:831 | command-filter-process-substitution-bypass

# Process substitution commands are not extracted by collect_shell_statements

## Finding

`collect_shell_statements` recursively screens inner commands only for backtick and `$(…)` forms. Bash process substitutions `<(…)` and `>(…)` are not parsed, so nested commands inside them never reach `check_command_line` while the shell still executes them.

## Violated Invariant Or Contract

`check_command` intends to validate every statement the shell would run from user input, including commands inside substitutions (covered for `$(…)` and backticks in tests).

## Oracle

`test_command_filter_blocks_chained_statements` asserts `echo $(rm -rf /)` is denied with pattern `^rm `, but no test covers `<(rm -rf /)`. Parser only recurses on `` ` `` and `$ (` at lines 831–867.

## Counterexample

Denylist `^rm `, command `cat <(rm -rf /)`. Filter checks one top-level statement that does not match `^rm `; bash runs `rm -rf /` inside the substitution.

## Why It Might Matter

Command filter bypass on bash/zsh panes where operators assume denylisted commands cannot run via allowed prefixes like `cat` or `git`.

## Proof

Dataflow trace: user command → `collect_shell_statements` (no `<(` handler) → single approved statement → wrapped execution → shell process substitution executes `rm`.

## Counterevidence Checked

Fish lacks process substitution; impact is shell-specific. Allowlist mode has the symmetric miss (allowed prefix smuggling disallowed inner command). Unquoted `;` chaining is separately fixed in the working tree.

## Suggested Next Step

Extend the parser to recurse into `<(…)` / `>(…)` bodies or reject process-substitution syntax at the policy layer; add denylist tests mirroring the `$(…)` cases.

## Status Notes

- 2026-06-27: open by Devana. Initial report written from static source inspection.
- 2026-07-05: fixed by extending `collect_shell_statements` to recursively screen bash/zsh process substitution bodies in `<(...)` and `>(...)`. Added `test_command_filter_blocks_process_substitution_statements`, which denies the original `cat <(rm -rf /)` counterexample under denylist `^rm ` and covers allowlist symmetry. Validation: `cargo test --lib security::tests::test_command_filter` passed.
- 2026-07-05: repaired validator YELLOW by skipping process-substitution recursion while inside double quotes, because bash/zsh treat quoted `<(...)` text as literal. Extended `test_command_filter_blocks_process_substitution_statements` to allow `echo "<(rm -rf /)"` under denylist `^rm ` while keeping unquoted `<(...)` and `>(...)` blocked. Validation: `cargo test --lib security::tests::test_command_filter` passed.

DEVANA-KEY: src/security.rs:831 | command-filter-process-substitution-bypass
DEVANA-SUMMARY: fixed | P1 | high | Commands inside <(…) process substitutions are now recursively screened by command_filter.
