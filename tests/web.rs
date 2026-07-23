use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use serde_json::json;
use tempfile::tempdir;
use tmux_mcp_rs::control::{
    set_gate_enabled, ActionRecord, ActionStatus, GateDecision, StatePaths,
};
use tmux_mcp_rs::security::SecurityPolicy;
use tmux_mcp_rs::web::{build_router, validate_bind_address, validate_key_input, HubState};
use tower::ServiceExt;

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
async fn human_keys_group_within_800ms_and_enter_closes_the_group() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");

    let first = state
        .record_human_input_at("%4", "a", true, 1_000)
        .await
        .expect("record a");
    let second = state
        .record_human_input_at("%4", "b", true, 1_700)
        .await
        .expect("record b");
    let enter = state
        .record_human_input_at("%4", "Enter", false, 1_800)
        .await
        .expect("record enter");
    let after_enter = state
        .record_human_input_at("%4", "c", true, 1_900)
        .await
        .expect("record c");

    assert_eq!(first.id, second.id);
    assert_eq!(first.id, enter.id);
    assert_ne!(enter.id, after_enter.id);
    assert_eq!(enter.arguments["text"], "ab\n");
    assert_eq!(after_enter.arguments["text"], "c");
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
    let app = build_router(state, "a".repeat(64), SecurityPolicy::default(), None);

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
    let app = build_router(state, "f".repeat(64), SecurityPolicy::default(), None);

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
    let app = build_router(state, "a".repeat(64), SecurityPolicy::default(), None);

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
    let app = build_router(state, token.clone(), SecurityPolicy::default(), None);

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
async fn api_rejects_non_json_and_oversized_mutations() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let token = "e".repeat(64);
    let app = build_router(state, token.clone(), SecurityPolicy::default(), None);

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
    let app = build_router(state, token.clone(), SecurityPolicy::default(), None);

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
async fn agent_authorize_returns_immediate_approval_when_gate_is_off() {
    let dir = tempdir().expect("temp state dir");
    let state = HubState::open(StatePaths::new(dir.path())).expect("open hub");
    let token = "c".repeat(64);
    let app = build_router(
        state.clone(),
        token.clone(),
        SecurityPolicy::default(),
        None,
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
