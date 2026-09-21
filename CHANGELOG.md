# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.1] - 2026-09-22

### Added
- Per-tool and foreground subprocess timings in the existing Web action log, plus a PowerShell summary script.
- Read-only `file-stat` and `gpu-snapshot` tools in the default tool group.

### Changed
- Uncertain execution stops agent modifications and asks it to report to the user; read-only inspection remains available and does not unlock the pane.
- `detach` remains opt-in and disables completion tracking only; it cannot bypass occupied panes or raw-mode policy and retains its pane lease.
- Long tasks should arrange application logging before launch. MCP does not archive full output or rerun commands to reconstruct lost history.
- Normal command tool responses omit duplicate command text unless `verbose:true`; command resources retain the full snapshot.
- MCP now routes calls through explicit SSH targets from `targets.toml`; target-qualified command resources keep per-target state separate.
- `execute-command` with `waitMs` now includes the current `CommandSnapshot` as `result`.
- `CommandSnapshot` schema version 2 adds `resultReady` to distinguish capture-in-progress from a finished but truncated capture.

### Fixed
- Failed/partial input delivery retains the uncertain command and pane protection instead of treating the pane as free.
- Pane occupancy and command results distinguish target accounts even when tmux IDs match.
- Caller wait budgets include recovery/capture time; expiry returns cached state without cancelling the remote command.
- Reject unspaced background operators while preserving shell redirections and `&&`.
- Recognize remote POSIX absolute buffer paths correctly when the MCP host is Windows.
- Bash tracked commands interrupted by Ctrl-C now finalize through a temporary prompt callback if SIGINT skips the normal command tail, preserving existing prompt hooks and INT traps. Sending Ctrl-C alone does not release a still-running command.
- Terminal waits and resource notifications now become ready only after the final bounded pane capture attempt, before the pane accepts another tracked command.
- Long-running commands no longer become `tracking_error` merely because the background tracking interval elapsed; caller `waitMs` expiry remains non-terminal and non-error.
- Command status JSON now emits `tracking_error` consistently while continuing to read the legacy `trackingerror` spelling.

## [0.6.0] - 2026-07-11

### Added
- Tracked commands return a stable `resourceUri` (`tmux://command/{commandId}/result`) from `execute-command`.
- MCP resource subscribe / `resources/updated` / `list_changed` for command lifecycle (preferred completion path).
- Optional `waitMs` on `execute-command` and `get-command-result` (timeout does not force a terminal status).
- Per-pane queue for tracked executes (`queued` → `running`).
- Shared `CommandSnapshot` JSON schema for tools and command resources (`schemaVersion: 1`).

### Changed
- Upgraded the RMCP server dependency to the 2.x API while preserving the existing server-only transport feature set.
- Command completion is side-channel based (`tmux set-buffer` + `wait-for`); scrollback START/DONE markers are debug/bracketing only and no longer authorize completion.
- Command statuses are now `queued`, `running`, `completed`, `failed`, `cancelled`, `tracking_error` (replacing `pending` / sticky `error` from marker parsing).
- Server instructions and skills prefer subscribe → notify → read over client poll loops.

### Security
- Closed DONE-marker spoof early completion: forging `TMUX_MCP_DONE_<id>_<code>` in pane text cannot complete a tracked command.

### Fixed
- Various tracked-command reliability fixes: command results now accurately report incomplete or truncated output across capture races, failures, and still-running commands.
- Fixed buffer-search pagination so boundary-spanning literal matches and cursor progression remain correct across pages.
- Enforced the same composite capability checks for resource discovery, reads, subscriptions, and resource updates.

## [0.5.0] - 2026-06-10
### Added
- Added `[security.tools]` and `TMUX_MCP_TOOLS` allow/deny filters for hiding and denying exact tools or tool groups at runtime.

### Changed
- Runtime-denied tools are pruned from the advertised MCP tool list instead of only failing when called.

## [0.4.0] - 2026-06-01
### Added
- Added `send-keys enter=true` for type-and-submit workflows.
- Added the `paste-text` tool for bracketed multi-line paste into interactive panes.
- Added CI coverage for tmux integration tests, including zsh bracketed-paste behavior.

### Changed
- Moved `paste-text` under the `interactive` feature gate with the other raw input tools.
- Documented hardened builds, paste behavior, memory limits, and tmux tab-delimited parsing limitations.
- Enforced the tmux 3.x startup requirement with a clearer version check.

### Fixed
- Passed leading-dash user values after `--` so tmux does not parse them as flags.
- Switched split sizing to the tmux 3.x-compatible `-l <percent>%` form.
- Bounded abandoned pending command tracking entries.
- Cleaned up temporary paste buffers when paste delivery fails.
- Rejected malformed `TMUX_MCP_SSH` values from the environment during startup.

### Security
- Quoted remote tmux commands for SSH execution so shell-sensitive socket and payload values preserve argv intent.
- Applied command filters to each non-empty pasted or submitted line, closing anchored regex bypasses for multi-line input.

## [0.3.0] - 2026-05-31
### Added
- Added the `send-hex` tool to send raw bytes via `tmux send-keys -H` (e.g. CSI-u sequences).
- Added a configurable `tracking_deadline_seconds` (default 600) for command tracking.
- Added the `interactive` and `special-keys` Cargo features (both on by default) so a
  hardened build can omit the raw keystroke tools entirely, leaving the filtered
  `execute-command` as the sole shell-input path. See the README "Hardened build" section.

### Changed
- Chunked `send-keys -H` payloads to stay within argv/tmux limits, mirroring literal send-keys chunking.
- Switched integration test panes to `/usr/bin/env bash --norc --noprofile` for portability on non-FHS systems.
- Clarified in the README that `send-hex` denylist screening is best-effort.

### Fixed
- Recovered command tracking when the START marker scrolls out of reach instead of marking the command as errored.
- Fixed sticky-`Error` status on still-running tracked commands.
- Fixed a hang in the capture backoff loop when the backoff factor was 1.

### Security
- Rejected line-editing control bytes (`0x08`, `0x15`, `0x17`, `0x7f`) in `send-hex` to prevent denylist bypasses.

## [0.2.1] - 2026-04-15
### Changed
- Upgraded `rmcp` from `1.2` to `1.4`.

## [0.2.0] - 2026-03-23
### Changed
- Upgraded `rmcp` from `0.14` to `1.2`.
- Migrated server metadata and read-resource handling to the `rmcp` 1.x constructor-based model API.
- Refreshed compatible dependency versions in `Cargo.lock`, including `tokio`, `clap`, `uuid`, `tempfile`, `tracing-subscriber`, `schemars`, `regex`, and `toml` 0.9.x.

## [0.1.3] - 2026-02-04
### Added
- Enforced session allowlist checks across pane/window tools and resources.
- Added configurable command tracking settings (capture limits and backoff).
- Added retention controls for completed command history.
- Added search streaming threshold for large buffer searches.

### Changed
- Bound command results to the resolved tmux socket and enforced socket policy on command reads.
- Improved literal send-keys performance with chunked fast paths for large payloads.

## [0.1.2] - 2026-01-26
### Changed
- Upgraded the optional rapidfuzz dependency to 0.5.0 for fuzzy similarity scoring.
- Improved the README

## [0.1.1] - 2026-01-26
### Added
- Added probe-and-refine workflow and tmux-buffer-explorer buffer tooling.

## [0.1.0] - 2026-01-24
- Initial Release
