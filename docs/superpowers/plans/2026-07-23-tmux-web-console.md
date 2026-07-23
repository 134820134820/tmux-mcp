# tmux MCP Web Console Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:executing-plans and superpowers:test-driven-development to
> implement this plan task-by-task.

**Goal:** Add a secure local tmux observer, interactive pane controller, and
optional approval Gate while retaining the current MCP execution path.

**Architecture:** A separate mode of the existing binary hosts a loopback HTTP
hub. Optional control clients wrap the existing tool router and forward action
snapshots; the hub stores them and serves a native single-page UI.

**Tech Stack:** Rust 1.70, Tokio, rmcp, axum 0.7, reqwest 0.12, native
HTML/CSS/JavaScript.

## Global Constraints

- Existing stdio/SSH/tmux behavior is unchanged without `--web-url`.
- Read-only tools never enter Gate or the activity log.
- Gate defaults off; off is fail-open and on is fail-closed for hub outages.
- Bind only to loopback and authenticate every API.
- No PTY, WebSocket, xterm, mouse protocol, database, or cross-client scheduler.

---

### Task 1: Control records and Gate state

**Files:** create `src/control.rs`; modify `Cargo.toml` and module declarations.

- [x] Write failing tests for state directory resolution, Gate file behavior,
      record classification, target extraction, and result snapshots.
- [x] Implement the minimum serializable types and optional HTTP control client.
- [x] Verify focused tests pass.

### Task 2: MCP tool interception

**Files:** modify `src/server.rs`.

- [x] Write failing tests proving read-only calls bypass control, mutating calls
      require decisions, rejection does not invoke the route, and tracked command
      terminal events update the same record.
- [x] Add the explicit `call_tool` wrapper and terminal event forwarding.
- [x] Verify server/control tests pass.

### Task 3: Local web service

**Files:** create `src/web.rs`; modify `src/main.rs`.

- [x] Write failing tests for loopback validation, token checks, Gate
      approve/reject, JSONL recovery/retention, and human key grouping.
- [x] Implement API state, persistence, tmux topology/capture/input, and CLI
      mode selection.
- [x] Verify web and CLI tests pass.

### Task 4: Single-page UI

**Files:** create `web/index.html`.

- [x] Add static-contract tests for required controls and safe text rendering.
- [x] Implement message/interactive switching, pane navigation, activity
      indicators, Gate dialog, operation log, polling, and keyboard mapping.
- [x] Verify focused API/UI tests and browser behavior.

### Task 5: Documentation, activation, and verification

**Files:** modify `README.md` and local deployment configuration.

- [x] Document generic, Codex, and Claude Code startup examples.
- [x] Run format, focused tests, all-target tests, clippy, and release build.
- [x] Start the hub locally, perform a dedicated-pane smoke test, and update only
      the current Codex tmux MCP entry after backing it up.
- [x] Review the diff against the approved design and record any baseline-only
      failures.
