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
        "800",
    ] {
        assert!(PAGE.contains(marker), "missing behavior marker: {marker}");
    }
}

#[test]
fn page_never_renders_remote_text_as_html() {
    assert!(PAGE.contains("textContent"));
    assert!(!PAGE.contains("innerHTML"));
    assert!(!PAGE.contains("insertAdjacentHTML"));
}
