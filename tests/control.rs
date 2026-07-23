use serde_json::json;
use tempfile::tempdir;
use tmux_mcp_rs::control::{
    default_state_dir, load_or_create_token, set_gate_enabled, validate_web_url, ActionKind,
    ActionRecord, ActionStatus, StatePaths,
};
use tmux_mcp_rs::types::{CommandSnapshot, CommandStatus};

#[test]
fn action_record_classifies_tools_and_extracts_exact_targets() {
    let record = ActionRecord::new(
        "Codex",
        "execute-command",
        json!({
            "socket": "/tmp/agent.sock",
            "sessionId": "$1",
            "windowId": "@2",
            "paneId": "%3",
            "command": "printf ok"
        }),
    );

    assert_eq!(record.kind, ActionKind::Command);
    assert_eq!(record.status, ActionStatus::Requested);
    assert_eq!(record.source, "Codex");
    assert_eq!(record.target.socket.as_deref(), Some("/tmp/agent.sock"));
    assert_eq!(record.target.session_id.as_deref(), Some("$1"));
    assert_eq!(record.target.window_id.as_deref(), Some("@2"));
    assert_eq!(record.target.pane_ids, vec!["%3"]);
    assert_eq!(
        record.target.pane_ids.first().map(String::as_str),
        Some("%3")
    );
}

#[test]
fn input_and_operation_tools_have_distinct_kinds() {
    let input = ActionRecord::new("Claude", "send-enter", json!({"paneId": "%9"}));
    let operation = ActionRecord::new("Claude", "split-pane", json!({"paneId": "%9"}));

    assert_eq!(input.kind, ActionKind::Input);
    assert_eq!(operation.kind, ActionKind::Operation);
}

#[test]
fn target_collects_both_panes_without_duplicates() {
    let record = ActionRecord::new(
        "Codex",
        "swap-pane",
        json!({
            "sourcePaneId": "%1",
            "targetPaneId": "%2",
            "paneId": "%1"
        }),
    );

    assert_eq!(record.target.pane_ids, vec!["%1", "%2"]);
}

#[test]
fn gate_file_is_absent_by_default_and_toggles_explicitly() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());

    assert!(!paths.gate_enabled());
    set_gate_enabled(&paths, true).expect("enable gate");
    assert!(paths.gate_enabled());
    set_gate_enabled(&paths, false).expect("disable gate");
    assert!(!paths.gate_enabled());
}

#[test]
fn action_lifecycle_keeps_one_record_and_attaches_terminal_command() {
    let mut record = ActionRecord::new(
        "Codex",
        "execute-command",
        json!({"paneId": "%3", "command": "printf ok"}),
    );
    let id = record.id.clone();

    record.mark_running();
    assert_eq!(record.status, ActionStatus::Running);

    record.mark_command(CommandSnapshot {
        command_id: "cmd-1".into(),
        resource_uri: "tmux://command/cmd-1/result".into(),
        status: CommandStatus::Completed,
        exit_code: Some(0),
        command: "printf ok".into(),
        pane_id: "%3".into(),
        socket: None,
        output: Some("ok".into()),
        output_truncated: false,
        elapsed_ms: 12,
        reason: None,
        wait_timed_out: None,
        schema_version: CommandSnapshot::SCHEMA_VERSION,
    });

    assert_eq!(record.id, id);
    assert_eq!(record.status, ActionStatus::Completed);
    assert_eq!(
        record
            .command_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.output.as_deref()),
        Some("ok")
    );
}

#[test]
fn token_is_created_once_and_reused() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());

    let first = load_or_create_token(&paths).expect("create token");
    let second = load_or_create_token(&paths).expect("reuse token");

    assert_eq!(first, second);
    assert_eq!(first.len(), 64);
}

#[test]
fn web_url_must_be_plain_http_on_loopback() {
    assert!(validate_web_url("http://127.0.0.1:38473").is_ok());
    assert!(validate_web_url("http://localhost:38473").is_ok());
    assert!(validate_web_url("https://127.0.0.1:38473").is_err());
    assert!(validate_web_url("http://example.com:38473").is_err());
}

#[test]
fn default_state_dir_has_a_tmux_mcp_leaf() {
    assert_eq!(
        default_state_dir()
            .file_name()
            .and_then(|name| name.to_str()),
        Some("tmux-mcp")
    );
}
