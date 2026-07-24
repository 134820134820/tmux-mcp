const PAGE: &str = include_str!("../web/index.html");

#[test]
fn page_contains_both_pane_modes_and_gate_controls() {
    for marker in [
        "mode-messages",
        "mode-interactive",
        "gate-toggle",
        "approval-dialog",
        "pane-tree",
        "message-list",
        "terminal-snapshot",
        "operation-log",
    ] {
        assert!(PAGE.contains(marker), "missing UI marker: {marker}");
    }
}

#[test]
fn page_polls_snapshots_and_uses_the_control_api() {
    for marker in [
        "/api/state",
        "/capture",
        "/keys",
        "/api/gate",
        "/api/approvals/",
        "250",
        "Enter 或切换 pane",
        "输入已清空",
    ] {
        assert!(PAGE.contains(marker), "missing behavior marker: {marker}");
    }
}

#[test]
fn message_mode_contains_a_single_line_command_composer() {
    for marker in [
        r#"id="command-composer""#,
        r#"id="command-input""#,
        r#"id="command-send""#,
        "/api/panes/${encodeURIComponent(paneId)}/commands",
        "requestSubmit()",
    ] {
        assert!(
            PAGE.contains(marker),
            "missing command composer marker: {marker}"
        );
    }
}

#[test]
fn command_composer_rejects_embedded_newlines() {
    assert!(PAGE.contains(r#"command.includes("\r") || command.includes("\n")"#));
}

#[test]
fn command_composer_only_restores_focus_in_message_mode() {
    assert!(PAGE.contains(r#"if (mode === "messages") elements.commandInput.focus();"#));
}

#[test]
fn page_never_renders_remote_text_as_html() {
    assert!(PAGE.contains("textContent"));
    assert!(!PAGE.contains("innerHTML"));
    assert!(!PAGE.contains("insertAdjacentHTML"));
}
