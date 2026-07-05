DEVANA-FINDING: v1
Priority: P1 | Confidence: high | Security-sensitive: yes | Status: fixed
Location: src/server.rs:2227-2232 | Slug: send-keys-literal-bypasses-command-filter

# send-keys literal mode skips check_command, bypassing the command allow/deny filter

## Finding

`send_keys` only runs the policy command filter when the request is non-literal:

```rust
let literal = input.0.literal.unwrap_or(false);
if !literal {
    if let Err(e) = self.policy.check_command(&input.0.keys) { ... }
}
```

Literal mode (`tmux send-keys -l`) types the `keys` string into the pane verbatim, which is exactly the mode that injects raw command text. Its sibling `send_hex` runs `check_command` unconditionally on the decoded bytes (src/server.rs:2327-2332) with an explicit comment that such input "can rewrite filtered input". So one tool enforces the filter on raw input and the other exempts the most direct raw-input path.

## Violated Invariant Or Contract

The configured `command_filter` (allowlist/denylist, `SecurityPolicy::check_command`, src/security.rs:659) is meant to gate command text injected into panes. A denylisted command must not be executable through a sibling tool by flipping one boolean. Note this is distinct from the already-reported `;` shell-chain bypass: that finding is about chaining inside an allowed command line; this is the literal flag skipping `check_command` entirely.

## Oracle

Neighboring implementation `send_hex` (src/server.rs:2310-2332) applies `check_command` to the decoded payload unconditionally, establishing the intended invariant that arbitrary raw input written to a pane is filtered. `send-keys --literal` is the verbatim-typing path and should be filtered at least as strictly.

## Counterexample

Policy with a denylist blocking `rm -rf`. Call:

```
send-keys { pane_id: "%1", keys: "rm -rf /important", literal: true, enter: true }
```

`literal == true` so `check_command` is skipped (line 2228). `tmux send-keys -l` types the string and the `enter` follow-up (src/server.rs:2272-2280) submits it. The identical payload with `literal: false`, or via `execute-command`, is rejected by `check_command`.

## Why It Might Matter

The command filter is a primary policy control for confining the agent. A trivially reachable bypass defeats denylists/allowlists for any deployment relying on `command_filter`, while `allow_send_keys` is on (the default for interactive builds).

## Proof

Cross-entry / control-flow mismatch. Line 2227 reads `literal`; line 2228 `if !literal` gates the only `check_command` call in the function (2229); the send path (2238-2266) calls `tmux::send_keys(..., literal, ...)` regardless; the `enter` follow-up at 2272 submits the line. Sibling `send_hex` filters unconditionally (2327), so the two raw-input tools disagree on enforcement.

## Counterevidence Checked

- `check_command` is a no-op only when `command_filter.mode == Off` or policy disabled (src/security.rs:632-678); under an active filter the bypass is real.
- No later unconditional `check_command` exists in `send_keys` (only call is at 2229).
- The `enter`/`repeat` path fires in literal mode (2272), so the typed text is actually submitted.
- The `literal` flag exists to send special characters verbatim, but nothing in docs marks it as an intentional filter exemption; the sibling tool proves the opposite intent.

## Suggested Next Step

Run `check_command(&input.0.keys)` unconditionally (regardless of `literal`), matching `send_hex`. If literal text legitimately needs to contain filtered substrings, gate that behind an explicit policy flag rather than the `literal` boolean.

## Agent Handoff

After working this report, preserve the original finding body. Update line 2 `Status: ...` and the final `DEVANA-SUMMARY:` status. Use one of: `open`, `fixed`, `invalid`, `stale`, `duplicate`, `wontfix`. Add dated notes below with the evidence checked.

## Status Notes

- 2026-07-05: fixed by applying `policy.check_command(&input.keys)` regardless of `literal` in `send_keys`. Added focused tests for literal `send-keys` denial under an active command filter and literal allow behavior when the key text passes the filter. Validated with `cargo test --bin tmux-mcp-rs send_keys`.
- 2026-06-25: open by Devana. Initial report written from static source inspection.

DEVANA-KEY: src/server.rs:2227 | P1 | send-keys-literal-bypasses-command-filter
DEVANA-SUMMARY: Status=fixed | P1 high src/server.rs:2227 - send-keys with literal=true now runs check_command, so command allow/deny filtering applies to literal and non-literal send-keys input.
