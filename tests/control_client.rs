use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use serde_json::json;
use tempfile::tempdir;
use tmux_mcp_rs::commands::CommandTracker;
use tmux_mcp_rs::control::{
    load_or_create_token, set_gate_enabled, ActionStatus, ControlClient, GateDecision, StatePaths,
};
use tmux_mcp_rs::security::SecurityPolicy;
use tmux_mcp_rs::types::{CommandSnapshot, CommandStatus, ShellType};
use tmux_mcp_rs::web::{build_router, HubState};

async fn start_hub(
    paths: StatePaths,
) -> (
    String,
    HubState,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let token = load_or_create_token(&paths).expect("token");
    let state = HubState::open(paths).expect("open hub");
    let app = build_router(
        state.clone(),
        token,
        SecurityPolicy::default(),
        None,
        Arc::new(CommandTracker::new(ShellType::Bash)),
    );
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind test hub");
    let address = listener.local_addr().expect("local address");
    let task = tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{address}"), state, task)
}

#[tokio::test]
async fn client_authorizes_through_the_real_loopback_api() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    let (url, state, task) = start_hub(paths.clone()).await;
    let client = ControlClient::new(&url, "Codex", paths).expect("control client");
    let record = client.action("send-keys", json!({"paneId": "%1", "keys": ["a"]}));

    assert_eq!(
        client.authorize(&record).await.expect("authorize"),
        GateDecision::Approved
    );
    assert_eq!(state.records_for_pane("%1").await.len(), 1);
    task.abort();
}

#[tokio::test]
async fn client_waits_for_gate_decision() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    set_gate_enabled(&paths, true).expect("enable gate");
    let (url, state, task) = start_hub(paths.clone()).await;
    let client = ControlClient::new(&url, "Claude", paths).expect("control client");
    let record = client.action("send-enter", json!({"paneId": "%2"}));
    let id = record.id.clone();

    let waiting = tokio::spawn({
        let client = client.clone();
        async move { client.authorize(&record).await }
    });
    for _ in 0..20 {
        if !state.pending().await.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(state.decide(&id, GateDecision::Approved).await);
    assert_eq!(
        waiting
            .await
            .expect("authorization task")
            .expect("decision"),
        GateDecision::Approved
    );
    task.abort();
}

#[tokio::test]
async fn unavailable_hub_is_fail_open_only_while_gate_is_off() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve address");
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let client = ControlClient::new(&url, "Codex", paths.clone()).expect("control client");
    let record = client.action("send-keys", json!({"paneId": "%3"}));

    assert_eq!(
        client.authorize(&record).await.expect("fail open"),
        GateDecision::Approved
    );
    set_gate_enabled(&paths, true).expect("enable gate");
    assert!(client.authorize(&record).await.is_err());
}

#[tokio::test]
async fn terminal_command_snapshot_updates_the_original_action() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    let (url, state, task) = start_hub(paths.clone()).await;
    let client = ControlClient::new(&url, "Codex", paths).expect("control client");
    let mut record = client.action(
        "execute-command",
        json!({"paneId": "%5", "command": "printf clean"}),
    );
    let action_id = record.id.clone();
    record.mark_running();
    client
        .track_command("command-1", record)
        .await
        .expect("track command");

    client
        .complete_command(CommandSnapshot {
            command_id: "command-1".into(),
            resource_uri: "tmux://command/command-1/result".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            command: "printf clean".into(),
            pane_id: "%5".into(),
            socket: None,
            output: None,
            output_truncated: true,
            elapsed_ms: 10,
            reason: None,
            wait_timed_out: None,
            schema_version: CommandSnapshot::SCHEMA_VERSION,
        })
        .await
        .expect("record terminal lifecycle");
    client
        .complete_command(CommandSnapshot {
            command_id: "command-1".into(),
            resource_uri: "tmux://command/command-1/result".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            command: "printf clean".into(),
            pane_id: "%5".into(),
            socket: None,
            output: Some("clean".into()),
            output_truncated: false,
            elapsed_ms: 20,
            reason: None,
            wait_timed_out: None,
            schema_version: CommandSnapshot::SCHEMA_VERSION,
        })
        .await
        .expect("complete command");

    let records = state.records_for_pane("%5").await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, action_id);
    assert_eq!(
        records[0]
            .command_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.output.as_deref()),
        Some("clean")
    );
    task.abort();
}

#[tokio::test]
async fn large_terminal_output_is_truncated_before_agent_record_upload() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    let (url, state, task) = start_hub(paths.clone()).await;
    let client = ControlClient::new(&url, "Codex", paths).expect("control client");
    let command = "x".repeat(900_000);
    let mut record = client.action(
        "execute-command",
        json!({"paneId": "%large", "command": command}),
    );
    record.mark_running();
    client
        .track_command("command-large", record)
        .await
        .expect("track command");

    client
        .complete_command(CommandSnapshot {
            command_id: "command-large".into(),
            resource_uri: "tmux://command/command-large/result".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            command: command.clone(),
            pane_id: "%large".into(),
            socket: None,
            output: Some("界".repeat(400_000)),
            output_truncated: false,
            elapsed_ms: 10,
            reason: None,
            wait_timed_out: None,
            schema_version: CommandSnapshot::SCHEMA_VERSION,
        })
        .await
        .expect("record bounded terminal lifecycle");

    let records = state.records_for_pane("%large").await;
    assert_eq!(records[0].status, ActionStatus::Completed);
    let snapshot = records[0].command_snapshot.as_ref().expect("snapshot");
    assert_eq!(snapshot.command, command);
    assert!(records[0].arguments.get("command").is_none());
    assert!(snapshot.output_truncated);
    assert!(snapshot.output.as_ref().unwrap().ends_with('界'));
    assert!(serde_json::to_vec(&records[0]).unwrap().len() < 1024 * 1024);
    task.abort();
}
