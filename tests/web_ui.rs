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
fn capture_response_is_scoped_to_the_requested_pane_and_mode() {
    let capture = PAGE
        .split_once("async function refreshCapture()")
        .expect("refreshCapture function")
        .1
        .split_once("function sendKey")
        .expect("sendKey function")
        .0;
    for marker in [
        "const paneId = selectedPane;",
        "/api/panes/${encodeURIComponent(paneId)}/capture",
        r#"mode === "interactive" && selectedPane === paneId"#,
    ] {
        assert!(
            capture.contains(marker),
            "missing scoped capture marker: {marker}"
        );
    }
    assert_eq!(
        capture
            .matches(r#"mode === "interactive" && selectedPane === paneId"#)
            .count(),
        2,
        "capture success and error rendering must both be scoped"
    );
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

#[test]
fn page_uses_the_light_theme_contract() {
    assert!(PAGE.contains(r#"name="color-scheme" content="light""#));
    assert!(PAGE.contains("--bg: #ffffff"));
    assert!(PAGE.contains("--panel-2: #f7f7f8"));
    assert!(PAGE.contains("color: #854d0e"));
    assert!(PAGE.contains(".truncated { color: #854d0e; }"));
    assert!(PAGE.contains("color: #b91c1c"));
    assert!(PAGE.contains("border-left: 3px solid var(--text)"));
    assert!(PAGE.contains("background: #ececf1"));
    assert!(!PAGE.contains("--bg: #0b0d10"));
}
