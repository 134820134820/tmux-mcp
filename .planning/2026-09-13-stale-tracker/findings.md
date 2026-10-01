# Findings

- `spawn_side_channel_watcher` waits forever on `tmux wait-for`; its checkpoint only checks whether the in-memory record still exists and never reads the durable exit-code buffer.
- `get-command-result` without `waitMs` uses `status_snapshot`, and the busy check uses `pane_running`; neither path reconciles a completed side channel.
- The wrapper writes the exit-code buffer before the DONE marker and signal, so a private buffer plus a matching DONE marker is sufficient recovery evidence without trusting arbitrary scrollback alone.
- Existing tests use a tmux stub whose `show-buffer` always returns `0`; recovery tests need an explicit missing-buffer switch so long-running cases do not look completed.
