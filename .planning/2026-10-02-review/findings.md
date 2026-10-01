# Findings
Scope includes persistent SSH, write-file, script execution, notifications, watcher recovery and Web UI.

Potential issues to reproduce: write-file bypasses command filters (only capability checked); lost watcher polling requires START/DONE even with valid exit buffer; ln destination lacks --; write failure returned as File not written although delivery uncertain.
UI regression script passes. SSH pool tests building offline in isolated target/test-build.

Confirmed findings:
- P1 security.rs:720: write-file inherits allow_execute_command but no command-filter or destination restriction. A policy allowing only ^ls$ still grants write-file. Diagnostic assertion fails.
- P1 commands.rs:958-966 + 980-991: dropped watcher enters polling that requires Complete START/DONE capture. With exit buffer 0 and lost START, wait times out with Running and retained pane lease. Diagnostic assertion fails.
- P2 server.rs:2662-2664: write transport failure is represented as File not written. Local fake SSH forwarded banner but damaged response after commit: file bytes exist while write_file returns ssh session protocol error. No uncertain-target state is established.
- P2 tmux.rs:1389: ln lacks --. Destination -tother is treated as target-directory option. Probe returns Ok(true), expected file absent, other/.tmux-mcp-write.* exists.

Validation: 5 existing ssh_pool tests passed; node tests/web_target_switch.mjs passed both groups. Three isolated regression assertions fail on actual source; one fault-injection diagnostic passes, proving committed write plus transport error. No full test suite or remote tests run.
