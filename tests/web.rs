use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use serde_json::json;
use tempfile::tempdir;
use tmux_mcp_rs::commands::CommandTracker;
use tmux_mcp_rs::control::{
    set_gate_enabled, ActionRecord, ActionStatus, GateDecision, StatePaths,
};
use tmux_mcp_rs::security::{SecurityConfig, SecurityPolicy};
use tmux_mcp_rs::types::{CommandSnapshot, CommandStatus, ShellType};
use tmux_mcp_rs::web::{build_router, validate_bind_address, validate_key_input, HubState};
use tower::ServiceExt;

fn test_tracker() -> Arc<CommandTracker> {
    Arc::new(CommandTracker::new(ShellType::Bash))
}

#[tokio::test]
async fn web_command_terminal_snapshot_updates_same_record() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let mut record = ActionRecord::new(
        "你",
        "execute-command",
        json!({"paneId": "%1", "command": "true"}),
    );
    record.mark_running();
    let record_id = record.id.clone();

    state
        .track_web_command("cmd-1".into(), record)
        .await
        .expect("track command");
    state
        .update_web_command(CommandSnapshot {
            command_id: "cmd-1".into(),
            resource_uri: "tmux://command/cmd-1/result".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            command: "true".into(),
            pane_id: "%1".into(),
            socket: None,
            output: Some("clean output\n".into()),
            output_truncated: false,
            elapsed_ms: 42,
            reason: None,
            wait_timed_out: None,
            schema_version: CommandSnapshot::SCHEMA_VERSION,
        })
        .await
        .expect("update command");

    let records = state.records_for_pane("%1").await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, record_id);
    assert_eq!(records[0].status, ActionStatus::Completed);
    let snapshot = records[0].command_snapshot.as_ref().expect("snapshot");
    assert_eq!(snapshot.output.as_deref(), Some("clean output\n"));
    assert_eq!(snapshot.exit_code, Some(0));
    assert_eq!(snapshot.elapsed_ms, 42);
    assert!(!snapshot.output_truncated);
}

#[tokio::test]
async fn gate_off_approves_immediately_and_persists_record() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    let state = HubState::open(paths).expect("open hub");
    let record = ActionRecord::new("Codex", "send-keys", json!({"paneId": "%1"}));

    let decision = state.authorize(record).await;

    assert_eq!(decision, GateDecision::Approved);
    let records = state.records_for_pane("%1").await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, ActionStatus::Approved);
}

#[tokio::test]
async fn gate_on_waits_until_the_exact_request_is_decided() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    set_gate_enabled(&paths, true).expect("enable gate");
    let state = HubState::open(paths).expect("open hub");
    let record = ActionRecord::new("Claude", "send-keys", json!({"paneId": "%2"}));
    let id = record.id.clone();

    let pending_state = state.clone();
    let task = tokio::spawn(async move { pending_state.authorize(record).await });
    tokio::task::yield_now().await;

    let pending = state.pending().await;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, id);
    assert!(state.decide(&id, GateDecision::Rejected).await);
    assert_eq!(
        task.await.expect("authorization task"),
        GateDecision::Rejected
    );
    assert_eq!(
        state.records_for_pane("%2").await[0].status,
        ActionStatus::Rejected
    );
}

#[tokio::test]
async fn disconnected_gate_request_is_pruned_as_incomplete() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    set_gate_enabled(&paths, true).expect("enable gate");
    let state = HubState::open(paths).expect("open hub");
    let record = ActionRecord::new("Codex", "send-keys", json!({"paneId": "%9"}));

    let pending_state = state.clone();
    let task = tokio::spawn(async move { pending_state.authorize(record).await });
    tokio::task::yield_now().await;
    task.abort();
    let _ = task.await;

    assert!(state.pending().await.is_empty());
    assert_eq!(
        state.records_for_pane("%9").await[0].status,
        ActionStatus::Incomplete
    );
}

#[tokio::test]
async fn jsonl_reload_uses_latest_snapshot_and_ignores_bad_tail() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    let state = HubState::open(paths.clone()).expect("open hub");
    let mut record = ActionRecord::new(
        "Codex",
        "execute-command",
        json!({"paneId": "%3", "command": "true"}),
    );
    state.upsert(record.clone()).await.expect("persist request");
    record.mark_running();
    state.upsert(record.clone()).await.expect("persist update");
    drop(state);

    let mut file = OpenOptions::new()
        .append(true)
        .open(&paths.events)
        .expect("open events");
    file.write_all(b"{incomplete").expect("append bad tail");

    let reloaded = HubState::open(paths).expect("reload hub");
    let records = reloaded.records_for_pane("%3").await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, record.id);
    assert_eq!(records[0].status, ActionStatus::Incomplete);
    drop(reloaded);

    let reloaded_again = HubState::open(StatePaths::new(dir.path())).expect("reload repaired tail");
    assert_eq!(
        reloaded_again.records_for_pane("%3").await[0].status,
        ActionStatus::Incomplete
    );
}

#[tokio::test]
async fn missing_main_recovers_complete_temporary_compaction_before_backup() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    let temporary = paths.events.with_extension("jsonl.tmp");
    let backup = paths.events.with_extension("jsonl.bak");
    let mut temporary_record =
        ActionRecord::new("Codex", "send-keys", json!({"paneId": "%temporary"}));
    temporary_record.mark_completed(None);
    let mut backup_record = ActionRecord::new("Codex", "send-keys", json!({"paneId": "%backup"}));
    backup_record.mark_completed(None);
    std::fs::write(
        &temporary,
        format!("{}\n", serde_json::to_string(&temporary_record).unwrap()),
    )
    .expect("write complete temporary store");
    std::fs::write(
        &backup,
        format!("{}\n", serde_json::to_string(&backup_record).unwrap()),
    )
    .expect("write backup store");

    let state = HubState::open(paths.clone()).expect("recover compacted store");

    assert_eq!(state.records_for_pane("%temporary").await.len(), 1);
    assert!(state.records_for_pane("%backup").await.is_empty());
    assert!(paths.events.is_file());
    assert!(!temporary.exists());
    assert!(!backup.exists());
}

#[tokio::test]
async fn upsert_recovers_interrupted_compaction_before_appending() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    let state = HubState::open(paths.clone()).expect("open hub");
    let mut before = ActionRecord::new("Codex", "send-keys", json!({"paneId": "%before"}));
    before.mark_completed(None);
    state.upsert(before).await.expect("persist earlier record");
    let temporary = paths.events.with_extension("jsonl.tmp");
    let backup = paths.events.with_extension("jsonl.bak");
    std::fs::copy(&paths.events, &temporary).expect("stage complete compacted store");
    std::fs::rename(&paths.events, &backup).expect("simulate interrupted publish");
    let mut after = ActionRecord::new("Codex", "send-keys", json!({"paneId": "%after"}));
    after.mark_completed(None);

    state.upsert(after).await.expect("append after recovery");
    drop(state);
    let reopened = HubState::open(paths).expect("reopen recovered store");

    assert_eq!(reopened.records_for_pane("%before").await.len(), 1);
    assert_eq!(reopened.records_for_pane("%after").await.len(), 1);
}

#[tokio::test]
async fn human_keys_stay_grouped_until_enter_even_after_a_long_pause() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");

    let first = state
        .record_human_input_at("%4", "a", true, 1_000)
        .await
        .expect("record a");
    let second = state
        .record_human_input_at("%4", "b", true, 10_000)
        .await
        .expect("record b");
    let enter = state
        .record_human_input_at("%4", "Enter", false, 20_000)
        .await
        .expect("record enter");
    let after_enter = state
        .record_human_input_at("%4", "c", true, 20_100)
        .await
        .expect("record c");

    assert_eq!(first.id, second.id);
    assert_eq!(first.id, enter.id);
    assert_ne!(enter.id, after_enter.id);
    assert_eq!(enter.arguments["text"], "ab\n");
    assert_eq!(after_enter.arguments["text"], "c");
}

#[tokio::test]
async fn human_backspace_edits_the_group_instead_of_logging_its_key_name() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");

    state
        .record_human_input_at("%4", "a", true, 1_000)
        .await
        .expect("record a");
    state
        .record_human_input_at("%4", "b", true, 1_100)
        .await
        .expect("record b");
    let backspace = state
        .record_human_input_at("%4", "BSpace", false, 1_200)
        .await
        .expect("record backspace");

    assert_eq!(backspace.arguments["text"], "a");
}

#[tokio::test]
async fn state_views_keep_only_the_latest_200_records() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");

    for index in 0..205_u64 {
        let mut message =
            ActionRecord::new("Codex", "send-keys", json!({"paneId": "%5", "keys": index}));
        message.requested_at_ms = index;
        message.updated_at_ms = index;
        state.upsert(message).await.expect("persist message");

        let mut operation = ActionRecord::new(
            "Codex",
            "select-pane",
            json!({"paneId": format!("%{index}")}),
        );
        operation.requested_at_ms = index;
        operation.updated_at_ms = index;
        state.upsert(operation).await.expect("persist operation");
    }

    let messages = state.records_for_pane("%5").await;
    let operations = state.operations().await;
    assert_eq!(messages.len(), 200);
    assert_eq!(messages[0].arguments["keys"], 5);
    assert_eq!(operations.len(), 200);
    assert_eq!(operations[0].arguments["paneId"], "%5");
}

#[tokio::test]
async fn oversized_jsonl_is_compacted_to_retained_records() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    let state = HubState::open(paths.clone()).expect("open hub");
    let payload = "x".repeat(10 * 1024);

    for index in 0..500_u64 {
        let mut operation = ActionRecord::new(
            "Codex",
            "select-layout",
            json!({"windowId": "@1", "layout": "tiled", "payload": payload}),
        );
        operation.requested_at_ms = index;
        operation.updated_at_ms = index;
        state.upsert(operation).await.expect("persist operation");
    }
    drop(state);

    assert!(std::fs::metadata(&paths.events).unwrap().len() < 4 * 1024 * 1024);
    let reloaded = HubState::open(paths).expect("reload compacted store");
    assert_eq!(reloaded.operations().await.len(), 200);
}

#[test]
fn bind_address_must_be_loopback() {
    assert!(validate_bind_address(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 38473)).is_ok());
    assert!(
        validate_bind_address(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 38473)).is_err()
    );
}

#[test]
fn interactive_key_input_is_small_and_explicit() {
    assert!(validate_key_input("hello 世界", true).is_ok());
    assert!(validate_key_input("Enter", false).is_ok());
    assert!(validate_key_input("C-c", false).is_ok());
    assert!(validate_key_input("C-z", false).is_err());
    assert!(validate_key_input(&"x".repeat(65_537), true).is_err());
}

#[tokio::test]
async fn index_rejects_dns_rebinding_host() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let app = build_router(
        state,
        "a".repeat(64),
        SecurityPolicy::default(),
        None,
        test_tracker(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::HOST, "example.com:38473")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn index_is_loopback_only_and_never_cached() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let app = build_router(
        state,
        "f".repeat(64),
        SecurityPolicy::default(),
        None,
        test_tracker(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::HOST, "127.0.0.1:38473")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
}

#[tokio::test]
async fn api_rejects_missing_token_and_non_loopback_host() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let app = build_router(
        state,
        "a".repeat(64),
        SecurityPolicy::default(),
        None,
        test_tracker(),
    );

    let missing_token = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/state?paneId=%251")
                .header(header::HOST, "127.0.0.1:38473")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_token.status(), StatusCode::UNAUTHORIZED);

    let bad_host = app
        .oneshot(
            Request::builder()
                .uri("/api/state?paneId=%251")
                .header(header::HOST, "example.com:38473")
                .header("x-tmux-mcp-token", "a".repeat(64))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_host.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn key_endpoint_rejects_unknown_special_key_before_touching_tmux() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let token = "d".repeat(64);
    let app = build_router(
        state,
        token.clone(),
        SecurityPolicy::default(),
        None,
        test_tracker(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/panes/%251/keys")
                .header(header::HOST, "127.0.0.1:38473")
                .header(header::ORIGIN, "http://127.0.0.1:38473")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-tmux-mcp-token", token)
                .body(Body::from(r#"{"key":"C-z","literal":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn command_endpoint_rejects_empty_and_multiline_commands() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let token = "g".repeat(64);
    let app = build_router(
        state.clone(),
        token.clone(),
        SecurityPolicy::default(),
        None,
        test_tracker(),
    );

    for command in ["   ", "printf one\nprintf two"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/panes/%251/commands")
                    .header(header::HOST, "127.0.0.1:38473")
                    .header(header::ORIGIN, "http://127.0.0.1:38473")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-tmux-mcp-token", token.clone())
                    .body(Body::from(json!({ "command": command }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert!(state.records_for_pane("%1").await.is_empty());
}

#[tokio::test]
async fn command_endpoint_rejects_65537_bytes_before_touching_tmux() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let token = "j".repeat(64);
    let app = build_router(
        state.clone(),
        token.clone(),
        SecurityPolicy::default(),
        None,
        test_tracker(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/panes/%251/commands")
                .header(header::HOST, "127.0.0.1:38473")
                .header(header::ORIGIN, "http://127.0.0.1:38473")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-tmux-mcp-token", token)
                .body(Body::from(
                    json!({ "command": "x".repeat(65_537) }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(state.records_for_pane("%1").await.is_empty());
}

#[tokio::test]
async fn command_endpoint_applies_existing_command_policy() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let token = "h".repeat(64);
    let config = SecurityConfig {
        allow_execute_command: false,
        ..SecurityConfig::default()
    };
    let policy = SecurityPolicy::from_config(config).unwrap();
    let app = build_router(
        state.clone(),
        token.clone(),
        policy,
        Some("must-not-be-called".into()),
        test_tracker(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/panes/%251/commands")
                .header(header::HOST, "127.0.0.1:38473")
                .header(header::ORIGIN, "http://127.0.0.1:38473")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-tmux-mcp-token", token)
                .body(Body::from(r#"{"command":"printf one"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(state.records_for_pane("%1").await.is_empty());
}

#[tokio::test]
async fn api_rejects_non_json_and_oversized_mutations() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let token = "e".repeat(64);
    let app = build_router(
        state,
        token.clone(),
        SecurityPolicy::default(),
        None,
        test_tracker(),
    );

    let non_json = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/gate")
                .header(header::HOST, "127.0.0.1:38473")
                .header(header::ORIGIN, "http://127.0.0.1:38473")
                .header(header::CONTENT_TYPE, "text/plain")
                .header("x-tmux-mcp-token", token.clone())
                .body(Body::from("enabled"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(non_json.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let oversized = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/gate")
                .header(header::HOST, "127.0.0.1:38473")
                .header(header::ORIGIN, "http://127.0.0.1:38473")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-tmux-mcp-token", token)
                .body(Body::from(vec![b'x'; 1024 * 1024 + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn browser_gate_toggle_requires_matching_origin_and_json() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    let state = HubState::open(paths.clone()).expect("open hub");
    let token = "b".repeat(64);
    let app = build_router(
        state,
        token.clone(),
        SecurityPolicy::default(),
        None,
        test_tracker(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/gate")
                .header(header::HOST, "127.0.0.1:38473")
                .header(header::ORIGIN, "http://127.0.0.1:38473")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-tmux-mcp-token", token)
                .body(Body::from(r#"{"enabled":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(paths.gate_enabled());
}

#[tokio::test]
async fn disabling_gate_approves_and_wakes_pending_requests() {
    let dir = tempdir().expect("temp state dir");
    let paths = StatePaths::new(dir.path());
    set_gate_enabled(&paths, true).expect("enable gate");
    let state = HubState::open(paths.clone()).expect("open hub");
    let token = "i".repeat(64);
    let app = build_router(
        state.clone(),
        token.clone(),
        SecurityPolicy::default(),
        None,
        test_tracker(),
    );
    let waiting = {
        let state = state.clone();
        tokio::spawn(async move {
            state
                .authorize(ActionRecord::new(
                    "Codex",
                    "send-keys",
                    json!({"paneId": "%pending"}),
                ))
                .await
        })
    };
    for _ in 0..100 {
        if !state.pending().await.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(state.pending().await.len(), 1);

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/gate")
                .header(header::HOST, "127.0.0.1:38473")
                .header(header::ORIGIN, "http://127.0.0.1:38473")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-tmux-mcp-token", token)
                .body(Body::from(r#"{"enabled":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!paths.gate_enabled());
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("authorization must be woken")
            .expect("authorization task"),
        GateDecision::Approved
    );
    assert!(state.pending().await.is_empty());
    assert_eq!(
        state.records_for_pane("%pending").await[0].status,
        ActionStatus::Approved
    );
}

#[tokio::test]
async fn agent_authorize_returns_immediate_approval_when_gate_is_off() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let token = "c".repeat(64);
    let app = build_router(
        state.clone(),
        token.clone(),
        SecurityPolicy::default(),
        None,
        test_tracker(),
    );
    let record = ActionRecord::new("Codex", "send-keys", json!({"paneId": "%7"}));

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/agent/authorize")
                .header(header::HOST, "127.0.0.1:38473")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-tmux-mcp-token", token)
                .body(Body::from(serde_json::to_vec(&record).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        state.records_for_pane("%7").await[0].status,
        ActionStatus::Approved
    );
}
