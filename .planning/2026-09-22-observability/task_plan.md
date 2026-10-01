# Agreed MCP improvements, local implementation only

## Authorized release follow-up
User now authorizes completing agreed changes, local executable replacement/update, commit and push. SSH reuse stays deferred. Pane anomalies should stop edits and ask the agent to report to the user, with no automatic cleanup/recovery. Keep detach false by default and retain pane safety. Preserve remote tasks. Existing product changes belong to this release; local planning and backups stay uncommitted.

1. Inspect and close pane-safety / detach gaps — complete.
2. Resolve output scope and validate source — complete: user excludes full-output storage from MCP; logging guidance only. Native checks and isolated fake-transport smoke tests passed; real Linux/SSH/GPU integration unavailable.
3. Build, back up and replace the local executable; check local activation boundaries — complete. v0.6.1 installed and re-probed, running clients preserved, Web log hub started locally. Client reconnection is required for new stdio code.
4. Update product documentation, commit and push to the existing origin — complete. ed8ba3c pushed normally to origin/main, including the pre-existing ancestor d7b6709. Tracked product worktree clean; local planning files remain untracked.

## Scope
Preserve existing uncommitted work. Do not connect to remote hosts, enable SSH reuse, replace the deployed executable, or stop existing processes. Explain SSH costs, unsafe pane reuse, and detached execution before changing those behaviors. User delegated opt-in saved-output details; the design remains unimplemented.

## Phases
1. Trace logging, subprocess, wait, read-only tools, and security paths — complete.
2. Implement measurements/statistics, concise pane reuse guidance, background-operator repair, bounded waiting, and file/GPU snapshots — complete.
3. Run local checks and isolated regressions; document output-storage proposal and remaining choices — complete.
4. Report changes, validation limits, and explanations — complete (handoff prepared).

## Completion boundaries
Installed executable unchanged (SHA256 886C398E7D790F750148C02341311BF3C88C487801F613F7ABCD47245F05F0F4). No remote connections, deployment, process restarts, SSH reuse, or saved-output implementation. Real SSH/GPU/tmux integration remains unverified. Command-echo behavior is confirmed; detach behavior remains unchanged for user discussion. Timing persistence uses the existing Web hub and is not active in the old deployed binary.

## Decisions
- No automatic session creation; give actionable hints.
- Keep rejecting actual shell comments in tracked commands.
- Item 2 confirmed: ordinary execute/result tool responses omit duplicate command by default, verbose restores it, and result resources retain the full snapshot. Detach remains under discussion.
- Measure current behavior before any SSH reuse experiment.
- Latest follow-up: quantify SSH benefit/cost and disconnect handling before deciding on reuse; assess partial input and abandoned panes before choosing a small safety change. Items 6 and 9 confirmed; item 7 details delegated.

## Errors
- Existing status checks require per-command Git safe.directory; global settings unchanged.
- Combined reads exceeded tool output budgets; use focused reads.
- cargo check --tests found 35 pre-existing missing initializer fields; supplied current defaults in tests only.
- Clippy identified a collapsible match and misplaced impl; corrected them, then annotated retained library-only refresh APIs after removing timeout-driven refresh from MCP waits.
