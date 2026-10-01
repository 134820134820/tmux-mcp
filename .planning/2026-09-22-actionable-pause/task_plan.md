# Actionable tracking pause recovery
- [x] Trace separate input-delivery pause and tracking-error lease paths
- [x] Add target/command-scoped human recovery with owning-client acknowledgement
- [x] Add inspect / confirm resume / pending / confirmed UI; preserve original pause controls
- [x] Verify exact target lease release, background acknowledgement without additional tool calls, unchanged uncertain result, replay safety, API auth/origin/confirmation, UI handlers
- [x] Build and deploy binary and Web; verify production page and rejection of an unconfirmed request

Installed SHA256: 8AB801E5F7C3E9BB3E211F572D563AA06FB80869E72EC9F00263AB565D6803AE
Web PID: 41764. Existing MCP clients 33568, 9248, 32804, 42892, 41240 preserved.
Backup: target/deployment-backups/tmux-mcp-before-actionable-pause-20260922.exe

Client activation remains: running old MCP processes must be reconnected in their owning apps to load new code. Legacy historical warnings explicitly disable recovery; API returns conflict rather than claiming an impossible hot recovery. No real pause was cleared and no remote input was sent.
