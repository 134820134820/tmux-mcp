# Stale tracked command investigation

## Goal

Confirm whether a completed long-running command can remain tracked as running, fix the shared root cause with the smallest safe change, and verify it.

## Next Step

Run the checks that are possible in this Windows workspace and review only the tracker diff.

### Phase 1: Inspect and reproduce

**Status:** complete

- [x] Trace all relevant callers and state transitions.
- [x] Identify why a DONE marker and exit buffer can coexist with a running record.

### Phase 2: Implement minimum root fix

**Status:** complete

- [x] Patch the shared tracker path.
- [x] Add one focused regression test.

### Phase 3: Verify

**Status:** complete

- [x] Run focused tests and formatting/checks.
- [x] Review the final diff.

## Decisions Made

| Decision | Reason |
|---|---|
| No force-release API unless recovery cannot be fixed centrally | Prefer self-healing existing behavior over another public surface. |

## Errors Encountered

| Error | Attempt | Resolution |
|---|---|---|
| Git refused dubious repository ownership | 1 | Use per-command `git -c safe.directory=E:/buyi_work/tmux-mcp`; do not mutate global config. |
| Cargo rejected multiple test filters | 1 | Run one `commands::tests::` filter covering the focused module. |
| Windows cannot execute the existing shell-script tmux stub | 1 | Record the limitation; verify formatting, library compilation, and a non-tmux unit test instead. |
| Test compilation has unrelated missing struct fields in `src/server.rs` | 1 | Leave existing working-tree changes untouched; use `cargo test --lib --no-run`. |
