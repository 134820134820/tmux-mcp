use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, State};
use axum::http::{
    header, uri::Authority, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode,
};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{oneshot, Mutex};

use crate::commands::{CommandEventKind, CommandTracker};
use crate::control::{
    set_gate_mode, ActionKind, ActionRecord, ActionStatus, AuthorizationResponse, GateDecision,
    GateMode, StatePaths, ACTION_SCHEMA_VERSION, AGENT_RECORD_MAX_BYTES,
};
use crate::security::SecurityPolicy;
use crate::tmux;
use crate::types::{CommandSnapshot, Pane, PaneInfo, Session, Window};

const PER_PANE_LIMIT: usize = 200;
const OPERATION_LIMIT: usize = 200;
const FULL_LOG_LIMIT: usize = 200;
const COMPACT_AT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_BODY_BYTES: usize = AGENT_RECORD_MAX_BYTES;
const TOKEN_HEADER: &str = "x-tmux-mcp-token";
const TOPOLOGY_CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct HubState {
    inner: Arc<HubInner>,
}

struct HubInner {
    paths: StatePaths,
    store: Mutex<RecordStore>,
    pending: Mutex<HashMap<String, PendingApproval>>,
    last_human: Mutex<Option<HumanGroup>>,
    web_commands: Mutex<HashMap<String, ActionRecord>>,
}

struct PendingApproval {
    record: ActionRecord,
    decision: oneshot::Sender<GateDecision>,
}

struct HumanGroup {
    id: String,
    pane_id: String,
}

struct RecordStore {
    path: std::path::PathBuf,
    records: HashMap<String, ActionRecord>,
}

#[derive(Clone)]
struct AppContext {
    hub: HubState,
    token: Arc<str>,
    policy: Arc<SecurityPolicy>,
    socket: Option<String>,
    tracker: Arc<CommandTracker>,
    topology_cache: Arc<Mutex<TopologyCache>>,
}

#[derive(Default)]
struct TopologyCache {
    value: Option<(Instant, Result<Topology, String>)>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateQuery {
    pane_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StateResponse {
    gate_enabled: bool,
    gate_mode: GateMode,
    pending: Vec<ActionRecord>,
    messages: Vec<ActionRecord>,
    operations: Vec<ActionRecord>,
    full_log: Vec<ActionRecord>,
    topology: Option<Topology>,
    topology_error: Option<String>,
    pane_info: Option<PaneInfo>,
    activity: HashMap<String, u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Topology {
    server_started_at_ms: Option<u64>,
    sessions: Vec<SessionNode>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionNode {
    session: Session,
    windows: Vec<WindowNode>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowNode {
    window: Window,
    panes: Vec<Pane>,
}

#[derive(Debug, Deserialize)]
struct GateUpdate {
    mode: GateMode,
}

#[derive(Debug, Deserialize)]
struct ApprovalInput {
    approved: bool,
}

#[derive(Debug, Serialize)]
struct OkResponse {
    ok: bool,
}

#[derive(Debug, Deserialize)]
struct KeyInput {
    key: String,
    literal: bool,
}

#[derive(Debug, Deserialize)]
struct CommandInput {
    command: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandAccepted {
    command_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureResponse {
    text: String,
    captured_at_ms: u64,
}

pub fn validate_bind_address(address: SocketAddr) -> Result<SocketAddr, String> {
    if address.ip().is_loopback() {
        Ok(address)
    } else {
        Err("web bind address must be loopback".into())
    }
}

pub fn validate_key_input(key: &str, literal: bool) -> Result<(), String> {
    if literal {
        if key.len() > 65_536 {
            return Err("literal input exceeds 64 KiB".into());
        }
        return Ok(());
    }

    const SPECIAL_KEYS: &[&str] = &[
        "Enter", "Up", "Down", "Left", "Right", "Tab", "Escape", "BSpace", "PageUp", "PageDown",
        "Home", "End", "C-c", "C-d",
    ];
    if SPECIAL_KEYS.contains(&key) {
        Ok(())
    } else {
        Err(format!("unsupported special key: {key}"))
    }
}

fn validate_command_input(command: &str) -> Result<(), &'static str> {
    if command.trim().is_empty() {
        return Err("command must not be empty");
    }
    if command.contains(['\n', '\r']) {
        return Err("command must be a single line");
    }
    if command.len() > 65_536 {
        return Err("command must not exceed 65536 bytes");
    }
    Ok(())
}

pub fn build_router(
    hub: HubState,
    token: String,
    policy: SecurityPolicy,
    socket: Option<String>,
    tracker: Arc<CommandTracker>,
) -> Router {
    let mut events = tracker.subscribe_events();
    let event_tracker = Arc::clone(&tracker);
    let event_hub = hub.clone();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event)
                    if event.kind == CommandEventKind::Terminal
                        || (event.kind == CommandEventKind::Updated
                            && event.status.is_terminal()) =>
                {
                    if let Some(execution) = event_tracker.get_command(&event.command_id).await {
                        let recorded = event_hub
                            .update_web_command(CommandSnapshot::from_execution(&execution, None))
                            .await
                            .unwrap_or(false);
                        if recorded && event.kind == CommandEventKind::Updated {
                            event_hub.forget_web_command(&event.command_id).await;
                        }
                    }
                }
                Ok(event) if event.kind == CommandEventKind::Evicted => {
                    event_hub.forget_web_command(&event.command_id).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    reconcile_web_commands(&event_hub, &event_tracker).await;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let context = AppContext {
        hub,
        token: Arc::from(token),
        policy: Arc::new(policy),
        socket,
        tracker,
        topology_cache: Arc::new(Mutex::new(TopologyCache::default())),
    };
    let api = Router::new()
        .route("/api/state", get(api_state))
        .route("/api/gate", put(api_gate))
        .route("/api/approvals/:id", put(api_approval))
        .route("/api/agent/authorize", post(agent_authorize))
        .route("/api/agent/record", post(agent_record))
        .route("/api/panes/:id/capture", get(api_capture))
        .route("/api/panes/:id/keys", post(api_keys))
        .route("/api/panes/:id/commands", post(send_command))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(context.clone(), api_guard));

    Router::new()
        .route("/", get(index))
        .merge(api)
        .with_state(context)
}

async fn reconcile_web_commands(hub: &HubState, tracker: &CommandTracker) {
    let command_ids = hub
        .inner
        .web_commands
        .lock()
        .await
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for command_id in command_ids {
        let Some(execution) = tracker.get_command(&command_id).await else {
            hub.forget_missing_web_command(&command_id).await;
            continue;
        };
        if !execution.status.is_terminal() {
            continue;
        }
        let presentation_ready = execution.output.is_some() || !execution.output_truncated;
        let recorded = hub
            .update_web_command(CommandSnapshot::from_execution(&execution, None))
            .await
            .unwrap_or(false);
        if recorded && presentation_ready {
            hub.forget_web_command(&command_id).await;
        }
    }
}

async fn index(State(context): State<AppContext>, headers: HeaderMap) -> Response {
    if !headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(host_is_loopback)
    {
        return (StatusCode::FORBIDDEN, "Host must be loopback").into_response();
    }

    let mut response =
        Html(include_str!("../web/index.html").replace("__TMUX_CONTROL_TOKEN__", &context.token))
            .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
             connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    response
}

async fn api_state(
    State(context): State<AppContext>,
    Query(query): Query<StateQuery>,
) -> Json<StateResponse> {
    let (topology, topology_error) = match cached_topology(&context).await {
        Ok(topology) => (Some(topology), None),
        Err(error) => (None, Some(error)),
    };
    let pane_exists = |pane_id: &str, topology: &Topology| {
        topology.sessions.iter().any(|session| {
            session
                .windows
                .iter()
                .any(|window| window.panes.iter().any(|pane| pane.id == pane_id))
        })
    };
    let messages = match (query.pane_id.as_deref(), topology.as_ref()) {
        (Some(pane_id), Some(topology)) if pane_exists(pane_id, topology) => {
            context
                .hub
                .records_for_pane_since(pane_id, topology.server_started_at_ms)
                .await
        }
        (Some(pane_id), None) => context.hub.records_for_pane(pane_id).await,
        _ => Vec::new(),
    };
    let pane_info = match (query.pane_id.as_deref(), topology.as_ref()) {
        (Some(pane_id), Some(topology)) if pane_exists(pane_id, topology) => {
            tmux::pane_info(pane_id, context.socket.as_deref())
                .await
                .ok()
        }
        _ => None,
    };
    let mut activity = HashMap::new();
    for record in context.hub.all_records().await {
        for pane_id in &record.target.pane_ids {
            activity
                .entry(pane_id.clone())
                .and_modify(|timestamp: &mut u64| {
                    *timestamp = (*timestamp).max(record.updated_at_ms)
                })
                .or_insert(record.updated_at_ms);
        }
    }
    Json(StateResponse {
        gate_enabled: context.hub.paths().gate_enabled(),
        gate_mode: context.hub.paths().gate_mode(),
        pending: context.hub.pending().await,
        messages,
        operations: context.hub.operations().await,
        full_log: context.hub.full_log().await,
        topology,
        topology_error,
        pane_info,
        activity,
    })
}

async fn api_gate(State(context): State<AppContext>, Json(input): Json<GateUpdate>) -> Response {
    match context.hub.set_gate_mode(input.mode).await {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("unable to update Gate: {error}"),
        )
            .into_response(),
    }
}

async fn api_approval(
    State(context): State<AppContext>,
    AxumPath(id): AxumPath<String>,
    Json(input): Json<ApprovalInput>,
) -> Response {
    let decision = if input.approved {
        GateDecision::Approved
    } else {
        GateDecision::Rejected
    };
    if context.hub.decide(&id, decision).await {
        Json(OkResponse { ok: true }).into_response()
    } else {
        (StatusCode::NOT_FOUND, "approval request not found").into_response()
    }
}

async fn agent_authorize(
    State(context): State<AppContext>,
    Json(record): Json<ActionRecord>,
) -> Json<AuthorizationResponse> {
    Json(AuthorizationResponse {
        decision: context.hub.authorize(record).await,
    })
}

async fn agent_record(
    State(context): State<AppContext>,
    Json(record): Json<ActionRecord>,
) -> Response {
    let refresh_topology = record_changes_topology(&record);
    match context.hub.upsert(record).await {
        Ok(()) => {
            if refresh_topology {
                context.topology_cache.lock().await.invalidate();
            }
            Json(OkResponse { ok: true }).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("unable to persist record: {error}"),
        )
            .into_response(),
    }
}

fn record_changes_topology(record: &ActionRecord) -> bool {
    record.kind == ActionKind::Operation
        && !record.read_only
        && record.status == ActionStatus::Completed
}

async fn api_capture(
    State(context): State<AppContext>,
    AxumPath(pane_id): AxumPath<String>,
) -> Response {
    if let Err(error) = validate_pane_id(&pane_id) {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    if let Err(error) = authorize_pane_action(&context, "capture-pane", &pane_id).await {
        return error;
    }
    match tmux::capture_pane(
        &pane_id,
        Some(200),
        false,
        None,
        None,
        true,
        context.socket.as_deref(),
    )
    .await
    {
        Ok(text) => Json(CaptureResponse {
            text,
            captured_at_ms: unix_time_ms(),
        })
        .into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            format!("unable to capture pane: {error}"),
        )
            .into_response(),
    }
}

async fn api_keys(
    State(context): State<AppContext>,
    AxumPath(pane_id): AxumPath<String>,
    Json(input): Json<KeyInput>,
) -> Response {
    if let Err(error) = validate_pane_id(&pane_id) {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    if let Err(error) = validate_key_input(&input.key, input.literal) {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    if let Err(error) = authorize_pane_action(&context, "send-keys", &pane_id).await {
        return error;
    }
    if let Err(error) = tmux::send_keys(
        &pane_id,
        &input.key,
        input.literal,
        context.socket.as_deref(),
    )
    .await
    {
        return (
            StatusCode::BAD_GATEWAY,
            format!("unable to send pane input: {error}"),
        )
            .into_response();
    }
    if let Err(error) = context
        .hub
        .record_human_input(&pane_id, &input.key, input.literal)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("input was sent but could not be recorded: {error}"),
        )
            .into_response();
    }
    Json(OkResponse { ok: true }).into_response()
}

async fn send_command(
    State(context): State<AppContext>,
    AxumPath(pane_id): AxumPath<String>,
    Json(input): Json<CommandInput>,
) -> Response {
    if let Err(error) = validate_pane_id(&pane_id) {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    if let Err(error) = validate_command_input(&input.command) {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    if let Err(error) = authorize_pane_action(&context, "execute-command", &pane_id).await {
        return error;
    }
    if let Err(error) = context.policy.check_command(&input.command) {
        return policy_response(error);
    }
    let mut record = ActionRecord::new(
        "你",
        "execute-command",
        json!({"paneId": pane_id, "command": input.command}),
    );
    record.mark_running();
    let command_id = match context
        .tracker
        .execute_command(
            &pane_id,
            &input.command,
            false,
            false,
            None,
            context.socket.clone(),
        )
        .await
    {
        Ok(command_id) => command_id,
        Err(error) => {
            record.mark_failed(Some(json!({"error": error.to_string()})));
            return match context.hub.upsert(record).await {
                Ok(()) => (
                    StatusCode::BAD_GATEWAY,
                    format!("unable to execute command: {error}"),
                )
                    .into_response(),
                Err(store_error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("command failed and could not be recorded: {store_error}"),
                )
                    .into_response(),
            };
        }
    };
    if let Err(error) = context
        .hub
        .track_web_command(command_id.clone(), record)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("command started but could not be recorded: {error}"),
        )
            .into_response();
    }
    if let Some(execution) = context.tracker.get_command(&command_id).await {
        if execution.status.is_terminal() {
            let presentation_ready = execution.output.is_some() || !execution.output_truncated;
            let recorded = context
                .hub
                .update_web_command(CommandSnapshot::from_execution(&execution, None))
                .await
                .unwrap_or(false);
            if recorded && presentation_ready {
                context.hub.forget_web_command(&command_id).await;
            }
        }
    }
    (StatusCode::ACCEPTED, Json(CommandAccepted { command_id })).into_response()
}

async fn cached_topology(context: &AppContext) -> Result<Topology, String> {
    let mut cache = context.topology_cache.lock().await;
    let now = Instant::now();
    if let Some(value) = cache.get(now) {
        return value;
    }
    let value = load_topology(context).await;
    cache.value = Some((Instant::now(), value.clone()));
    value
}

impl TopologyCache {
    fn get(&self, now: Instant) -> Option<Result<Topology, String>> {
        self.value
            .as_ref()
            .filter(|(loaded_at, _)| now.duration_since(*loaded_at) < TOPOLOGY_CACHE_TTL)
            .map(|(_, value)| value.clone())
    }

    fn invalidate(&mut self) {
        self.value = None;
    }
}

async fn load_topology(context: &AppContext) -> Result<Topology, String> {
    for tool in ["list-sessions", "list-windows", "list-panes"] {
        context
            .policy
            .check_tool(tool)
            .map_err(|error| error.to_string())?;
    }
    context
        .policy
        .check_socket(context.socket.as_deref())
        .map_err(|error| error.to_string())?;

    let sessions = tmux::list_sessions(context.socket.as_deref())
        .await
        .map_err(|error| error.to_string())?;
    let server_started_at_ms = tmux::server_start_time(context.socket.as_deref())
        .await
        .ok()
        .flatten()
        .map(|seconds| seconds.saturating_mul(1000));
    let mut nodes = Vec::new();
    for session in sessions {
        if context
            .policy
            .check_session_identity(&session.id, Some(&session.name))
            .is_err()
        {
            continue;
        }
        let windows = tmux::list_windows(&session.id, context.socket.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        let mut window_nodes = Vec::new();
        for window in windows {
            let panes = tmux::list_panes(&window.id, context.socket.as_deref())
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter(|pane| context.policy.check_pane(&pane.id).is_ok())
                .collect();
            window_nodes.push(WindowNode { window, panes });
        }
        nodes.push(SessionNode {
            session,
            windows: window_nodes,
        });
    }
    Ok(Topology {
        server_started_at_ms,
        sessions: nodes,
    })
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::types::{CommandExecution, CommandStatus, ShellType};
    use std::time::Instant;

    #[test]
    fn topology_cache_reuses_fresh_errors_and_expires_stale_data() {
        let loaded_at = Instant::now();
        let cache = TopologyCache {
            value: Some((loaded_at, Err("tmux unavailable".into()))),
        };

        assert!(matches!(
            cache.get(loaded_at + Duration::from_secs(4)),
            Some(Err(error)) if error == "tmux unavailable"
        ));
        assert!(cache.get(loaded_at + Duration::from_secs(5)).is_none());
    }

    #[test]
    fn only_completed_mutating_operations_refresh_topology() {
        let mut split = ActionRecord::new("Codex", "split-pane", json!({"paneId": "%1"}));
        assert!(!record_changes_topology(&split));
        split.mark_completed(None);
        assert!(record_changes_topology(&split));

        let mut read_only = ActionRecord::new("Codex", "list-panes", json!({"windowId": "@1"}));
        read_only.read_only = true;
        read_only.mark_completed(None);
        assert!(!record_changes_topology(&read_only));

        let mut input = ActionRecord::new("Codex", "send-keys", json!({"paneId": "%1"}));
        input.mark_completed(None);
        assert!(!record_changes_topology(&input));
    }

    #[test]
    fn command_input_limit_is_measured_in_bytes() {
        let accepted = "界".repeat(21_845) + "a";
        let rejected = "界".repeat(21_845) + "ab";
        assert!(validate_command_input(&accepted).is_ok());
        assert!(validate_command_input(&rejected).is_err());
    }

    #[tokio::test]
    async fn web_command_mapping_is_published_after_running_record_is_persisted() {
        let dir = tempfile::tempdir().expect("temp state dir");
        let hub = HubState::open(StatePaths::new(dir.path())).expect("open hub");
        let store = hub.inner.store.lock().await;
        let mut record = ActionRecord::new(
            "你",
            "execute-command",
            json!({"paneId": "%1", "command": "true"}),
        );
        record.mark_running();
        let registering = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.track_web_command("cmd-1".into(), record)
                    .await
                    .expect("track command");
            })
        };
        tokio::task::yield_now().await;

        assert!(!hub.inner.web_commands.lock().await.contains_key("cmd-1"));

        drop(store);
        registering.await.expect("registration task");
        assert!(hub.inner.web_commands.lock().await.contains_key("cmd-1"));
    }

    #[tokio::test]
    async fn authorize_rechecks_gate_after_waiting_for_pending_lock() {
        let dir = tempfile::tempdir().expect("temp state dir");
        let paths = StatePaths::new(dir.path());
        set_gate_mode(&paths, GateMode::Approval).expect("enable gate");
        let hub = HubState::open(paths.clone()).expect("open hub");
        let pending = hub.inner.pending.lock().await;
        let authorizing = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.authorize(ActionRecord::new(
                    "Codex",
                    "send-keys",
                    json!({"paneId": "%race"}),
                ))
                .await
            })
        };
        for _ in 0..100 {
            if hub
                .records_for_pane("%race")
                .await
                .first()
                .is_some_and(|record| {
                    record.status == crate::control::ActionStatus::WaitingApproval
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }

        set_gate_mode(&paths, GateMode::Off).expect("disable gate");
        drop(pending);

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), authorizing)
                .await
                .expect("authorization must not remain pending")
                .expect("authorization task"),
            GateDecision::Approved
        );
        assert!(hub.pending().await.is_empty());
        assert_eq!(
            hub.records_for_pane("%race").await[0].status,
            crate::control::ActionStatus::Approved
        );
    }

    #[tokio::test]
    async fn gate_enable_waits_for_pending_transition_lock() {
        let dir = tempfile::tempdir().expect("temp state dir");
        let paths = StatePaths::new(dir.path());
        let hub = HubState::open(paths.clone()).expect("open hub");
        let pending = hub.inner.pending.lock().await;
        let enabling = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.set_gate_mode(GateMode::Approval).await })
        };
        tokio::task::yield_now().await;

        assert!(!paths.gate_enabled());

        drop(pending);
        enabling.await.expect("enable task").expect("enable gate");
        assert!(paths.gate_enabled());
    }

    #[tokio::test]
    async fn gate_off_authorize_waits_for_pending_transition_lock() {
        let dir = tempfile::tempdir().expect("temp state dir");
        let hub = HubState::open(StatePaths::new(dir.path())).expect("open hub");
        let pending = hub.inner.pending.lock().await;
        let authorizing = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.authorize(ActionRecord::new(
                    "Codex",
                    "send-keys",
                    json!({"paneId": "%off-race"}),
                ))
                .await
            })
        };
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if authorizing.is_finished() {
                break;
            }
        }

        assert!(!authorizing.is_finished());

        drop(pending);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), authorizing)
                .await
                .expect("authorization must complete after lock release")
                .expect("authorization task"),
            GateDecision::Approved
        );
        assert_eq!(
            hub.records_for_pane("%off-race").await[0].status,
            crate::control::ActionStatus::Approved
        );
    }

    #[tokio::test]
    async fn lag_reconciliation_updates_tracked_web_commands() {
        let dir = tempfile::tempdir().expect("temp state dir");
        let hub = HubState::open(StatePaths::new(dir.path())).expect("open hub");
        let mut record = ActionRecord::new(
            "你",
            "execute-command",
            json!({"paneId": "%lagged", "command": "true"}),
        );
        record.mark_running();
        hub.track_web_command("cmd-lagged".into(), record)
            .await
            .expect("track command");
        let mut evicted = ActionRecord::new(
            "你",
            "execute-command",
            json!({"paneId": "%evicted", "command": "true"}),
        );
        evicted.mark_running();
        hub.track_web_command("cmd-evicted".into(), evicted)
            .await
            .expect("track evicted command");
        let tracker = CommandTracker::new(ShellType::Bash);
        let now = Instant::now();
        tracker
            .insert_test_execution(CommandExecution {
                id: "cmd-lagged".into(),
                pane_id: "%lagged".into(),
                socket: None,
                command: "true".into(),
                status: CommandStatus::Completed,
                exit_code: Some(0),
                output: Some("done".into()),
                output_truncated: false,
                reason: None,
                started_at: now,
                completed_at: Some(now),
                raw_mode: false,
                tracking_disabled: false,
            })
            .await;

        reconcile_web_commands(&hub, &tracker).await;

        assert_eq!(
            hub.records_for_pane("%lagged").await[0].status,
            crate::control::ActionStatus::Completed
        );
        assert!(!hub
            .inner
            .web_commands
            .lock()
            .await
            .contains_key("cmd-lagged"));
        assert!(!hub
            .inner
            .web_commands
            .lock()
            .await
            .contains_key("cmd-evicted"));
        assert_eq!(
            hub.records_for_pane("%evicted").await[0].status,
            crate::control::ActionStatus::Incomplete
        );
    }
}

async fn authorize_pane_action(
    context: &AppContext,
    tool: &str,
    pane_id: &str,
) -> Result<(), Response> {
    context
        .policy
        .check_tool(tool)
        .and_then(|_| context.policy.check_socket(context.socket.as_deref()))
        .and_then(|_| context.policy.check_pane(pane_id))
        .map_err(policy_response)?;

    if context.policy.has_session_allowlist() {
        let info = tmux::pane_info(pane_id, context.socket.as_deref())
            .await
            .map_err(|error| {
                (
                    StatusCode::FORBIDDEN,
                    format!("unable to resolve pane session: {error}"),
                )
                    .into_response()
            })?;
        if context.policy.check_session(&info.session_id).is_err() {
            let sessions = tmux::list_sessions(context.socket.as_deref())
                .await
                .map_err(|error| {
                    (
                        StatusCode::FORBIDDEN,
                        format!("unable to resolve pane session: {error}"),
                    )
                        .into_response()
                })?;
            let session = sessions
                .iter()
                .find(|session| session.id == info.session_id)
                .ok_or_else(|| {
                    (StatusCode::FORBIDDEN, "pane session is not allowed").into_response()
                })?;
            context
                .policy
                .check_session_identity(&session.id, Some(&session.name))
                .map_err(policy_response)?;
        }
    }
    Ok(())
}

fn policy_response(error: crate::errors::Error) -> Response {
    (StatusCode::FORBIDDEN, error.to_string()).into_response()
}

fn validate_pane_id(pane_id: &str) -> Result<(), String> {
    let digits = pane_id
        .strip_prefix('%')
        .ok_or_else(|| "pane id must start with '%'".to_string())?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("pane id must be '%' followed by digits".into());
    }
    Ok(())
}

async fn api_guard(
    State(context): State<AppContext>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let headers = request.headers();
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "Host header is required").into_response();
    };
    if !host_is_loopback(host) {
        return (StatusCode::FORBIDDEN, "Host must be loopback").into_response();
    }

    let authorized = headers
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.as_bytes() == context.token.as_bytes())
        .unwrap_or(false);
    if !authorized {
        return (StatusCode::UNAUTHORIZED, "invalid control token").into_response();
    }

    if matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH
    ) {
        let json_content = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .map(str::trim)
                    .map(|mime| mime.eq_ignore_ascii_case("application/json"))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !json_content {
            return (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
            )
                .into_response();
        }

        if !request.uri().path().starts_with("/api/agent/") {
            let expected = format!("http://{host}");
            let origin_matches = headers
                .get(header::ORIGIN)
                .and_then(|value| value.to_str().ok())
                .map(|origin| origin.eq_ignore_ascii_case(&expected))
                .unwrap_or(false);
            if !origin_matches {
                return (StatusCode::FORBIDDEN, "Origin does not match Host").into_response();
            }
        }
    }

    next.run(request).await
}

fn host_is_loopback(host: &str) -> bool {
    let Ok(authority) = Authority::from_str(host) else {
        return false;
    };
    let host = authority.host();
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

impl HubState {
    pub fn open(paths: StatePaths) -> io::Result<Self> {
        fs::create_dir_all(&paths.directory)?;
        let mut store = RecordStore::open(paths.events.clone())?;
        store.mark_interrupted_incomplete()?;
        Ok(Self {
            inner: Arc::new(HubInner {
                paths,
                store: Mutex::new(store),
                pending: Mutex::new(HashMap::new()),
                last_human: Mutex::new(None),
                web_commands: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn paths(&self) -> &StatePaths {
        &self.inner.paths
    }

    pub async fn upsert(&self, record: ActionRecord) -> io::Result<()> {
        self.inner.store.lock().await.upsert(record)
    }

    pub async fn track_web_command(
        &self,
        command_id: String,
        record: ActionRecord,
    ) -> io::Result<()> {
        self.upsert(record.clone()).await?;
        self.inner
            .web_commands
            .lock()
            .await
            .insert(command_id, record);
        Ok(())
    }

    pub async fn update_web_command(&self, snapshot: CommandSnapshot) -> io::Result<bool> {
        let mut commands = self.inner.web_commands.lock().await;
        let Some(record) = commands.get_mut(&snapshot.command_id) else {
            return Ok(false);
        };
        record.mark_command(snapshot);
        self.upsert(record.clone()).await?;
        Ok(true)
    }

    pub async fn forget_web_command(&self, command_id: &str) {
        self.inner.web_commands.lock().await.remove(command_id);
    }

    async fn forget_missing_web_command(&self, command_id: &str) {
        let Some(mut record) = self.inner.web_commands.lock().await.remove(command_id) else {
            return;
        };
        if matches!(
            record.status,
            crate::control::ActionStatus::Requested
                | crate::control::ActionStatus::WaitingApproval
                | crate::control::ActionStatus::Approved
                | crate::control::ActionStatus::Running
        ) {
            record.mark_incomplete();
            let _ = self.upsert(record).await;
        }
    }

    pub async fn authorize(&self, mut record: ActionRecord) -> GateDecision {
        let mut pending = self.inner.pending.lock().await;
        if !self.inner.paths.gate_enabled() {
            record.mark_approved();
            let _ = self.upsert(record).await;
            return GateDecision::Approved;
        }

        record.mark_waiting_approval();
        let _ = self.upsert(record.clone()).await;
        let id = record.id.clone();
        let (sender, receiver) = oneshot::channel();
        pending.insert(
            id,
            PendingApproval {
                record,
                decision: sender,
            },
        );
        drop(pending);
        receiver.await.unwrap_or(GateDecision::Rejected)
    }

    async fn set_gate_mode(&self, mode: GateMode) -> io::Result<()> {
        if mode != GateMode::Off {
            let _pending = self.inner.pending.lock().await;
            return set_gate_mode(&self.inner.paths, mode);
        }
        let approvals = {
            let mut pending = self.inner.pending.lock().await;
            set_gate_mode(&self.inner.paths, GateMode::Off)?;
            pending
                .drain()
                .map(|(_, approval)| approval)
                .collect::<Vec<_>>()
        };
        let mut first_error = None;
        for mut approval in approvals {
            approval.record.mark_approved();
            if let Err(error) = self.upsert(approval.record).await {
                first_error.get_or_insert(error);
            }
            let _ = approval.decision.send(GateDecision::Approved);
        }
        first_error.map_or(Ok(()), Err)
    }

    pub async fn decide(&self, id: &str, decision: GateDecision) -> bool {
        let Some(mut pending) = self.inner.pending.lock().await.remove(id) else {
            return false;
        };
        if pending.decision.is_closed() {
            pending.record.mark_incomplete();
            let _ = self.upsert(pending.record).await;
            return false;
        }
        match decision {
            GateDecision::Approved => pending.record.mark_approved(),
            GateDecision::Rejected => pending.record.mark_rejected(),
        }
        let _ = self.upsert(pending.record).await;
        let _ = pending.decision.send(decision);
        true
    }

    pub async fn pending(&self) -> Vec<ActionRecord> {
        let (mut records, disconnected) = {
            let mut pending = self.inner.pending.lock().await;
            let disconnected_ids: Vec<_> = pending
                .iter()
                .filter(|(_, approval)| approval.decision.is_closed())
                .map(|(id, _)| id.clone())
                .collect();
            let disconnected = disconnected_ids
                .into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect::<Vec<_>>();
            let records: Vec<ActionRecord> = pending
                .values()
                .map(|approval| approval.record.clone())
                .collect();
            (records, disconnected)
        };
        for mut approval in disconnected {
            approval.record.mark_incomplete();
            let _ = self.upsert(approval.record).await;
        }
        records.sort_by_key(|record| record.requested_at_ms);
        records
    }

    pub async fn records_for_pane(&self, pane_id: &str) -> Vec<ActionRecord> {
        self.records_for_pane_since(pane_id, None).await
    }

    pub async fn records_for_pane_since(
        &self,
        pane_id: &str,
        minimum_requested_at_ms: Option<u64>,
    ) -> Vec<ActionRecord> {
        self.inner
            .store
            .lock()
            .await
            .records_for_pane_since(pane_id, minimum_requested_at_ms)
    }

    pub async fn operations(&self) -> Vec<ActionRecord> {
        self.inner.store.lock().await.operations()
    }

    pub async fn full_log(&self) -> Vec<ActionRecord> {
        self.inner.store.lock().await.full_log()
    }

    pub async fn all_records(&self) -> Vec<ActionRecord> {
        self.inner.store.lock().await.sorted_records()
    }

    pub async fn record_human_input_at(
        &self,
        pane_id: &str,
        key: &str,
        literal: bool,
        at_ms: u64,
    ) -> io::Result<ActionRecord> {
        let mut last = self.inner.last_human.lock().await;
        let mut store = self.inner.store.lock().await;
        let reusable_id = last
            .as_ref()
            .and_then(|group| (group.pane_id == pane_id).then(|| group.id.clone()));

        let mut record = reusable_id
            .and_then(|id| store.records.get(&id).cloned())
            .unwrap_or_else(|| {
                let mut record = ActionRecord::new(
                    "你",
                    "web-input",
                    json!({"paneId": pane_id, "text": "", "literal": literal}),
                );
                record.requested_at_ms = at_ms;
                record
            });
        let mut text = record
            .arguments
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match (literal, key) {
            (true, key) => text.push_str(key),
            (false, "Enter") => text.push('\n'),
            (false, "BSpace") => {
                text.pop();
            }
            (false, key) => text.push_str(key),
        }
        record.arguments["text"] = Value::String(text);
        record.arguments["literal"] = Value::Bool(literal);
        record.updated_at_ms = at_ms;
        record.mark_completed(None);
        record.updated_at_ms = at_ms;
        store.upsert(record.clone())?;
        *last = if !literal && key == "Enter" {
            None
        } else {
            Some(HumanGroup {
                id: record.id.clone(),
                pane_id: pane_id.to_owned(),
            })
        };
        Ok(record)
    }

    pub async fn record_human_input(
        &self,
        pane_id: &str,
        key: &str,
        literal: bool,
    ) -> io::Result<ActionRecord> {
        self.record_human_input_at(pane_id, key, literal, unix_time_ms())
            .await
    }
}

impl RecordStore {
    fn open(path: std::path::PathBuf) -> io::Result<Self> {
        recover_compaction(&path)?;
        let mut records = HashMap::new();
        match File::open(&path) {
            Ok(file) => {
                for line in BufReader::new(file).lines() {
                    let Ok(line) = line else {
                        continue;
                    };
                    let Ok(record) = serde_json::from_str::<ActionRecord>(&line) else {
                        continue;
                    };
                    if record.schema_version == ACTION_SCHEMA_VERSION {
                        records.insert(record.id.clone(), record);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        repair_partial_tail(&path)?;
        Ok(Self { path, records })
    }

    fn mark_interrupted_incomplete(&mut self) -> io::Result<()> {
        let interrupted: Vec<_> = self
            .records
            .values()
            .filter(|record| {
                matches!(
                    record.status,
                    crate::control::ActionStatus::Requested
                        | crate::control::ActionStatus::WaitingApproval
                        | crate::control::ActionStatus::Approved
                        | crate::control::ActionStatus::Running
                )
            })
            .cloned()
            .collect();
        for mut record in interrupted {
            record.mark_incomplete();
            self.upsert(record)?;
        }
        Ok(())
    }

    fn upsert(&mut self, record: ActionRecord) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        recover_compaction(&self.path)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, &record)?;
        file.write_all(b"\n")?;
        file.flush()?;
        let should_compact = file.metadata()?.len() > COMPACT_AT_BYTES;
        drop(file);
        self.records.insert(record.id.clone(), record);
        if should_compact {
            self.compact()?;
        }
        Ok(())
    }

    fn sorted_records(&self) -> Vec<ActionRecord> {
        let mut records: Vec<_> = self.records.values().cloned().collect();
        sort_records(&mut records);
        records
    }

    fn records_for_pane(&self, pane_id: &str) -> Vec<ActionRecord> {
        self.records_for_pane_since(pane_id, None)
    }

    fn records_for_pane_since(
        &self,
        pane_id: &str,
        minimum_requested_at_ms: Option<u64>,
    ) -> Vec<ActionRecord> {
        let mut records: Vec<_> = self
            .records
            .values()
            .filter(|record| {
                (record.kind != ActionKind::Operation
                    || matches!(
                        record.tool.as_str(),
                        "list-directory"
                            | "read-file"
                            | "find-files"
                            | "search-text"
                            | "git-status"
                            | "git-diff"
                            | "git-log"
                            | "git-show"
                    ))
                    && minimum_requested_at_ms
                        .map_or(true, |minimum| record.requested_at_ms >= minimum)
                    && record
                        .target
                        .pane_ids
                        .iter()
                        .any(|target| target == pane_id)
            })
            .cloned()
            .collect();
        sort_records(&mut records);
        if records.len() > PER_PANE_LIMIT {
            records.drain(..records.len() - PER_PANE_LIMIT);
        }
        records
    }

    fn operations(&self) -> Vec<ActionRecord> {
        let mut records: Vec<_> = self
            .records
            .values()
            .filter(|record| record.kind == ActionKind::Operation && !record.read_only)
            .cloned()
            .collect();
        sort_records(&mut records);
        if records.len() > OPERATION_LIMIT {
            records.drain(..records.len() - OPERATION_LIMIT);
        }
        records
    }

    fn full_log(&self) -> Vec<ActionRecord> {
        let mut records: Vec<_> = self
            .records
            .values()
            .filter(|record| record.tool != "execute-command" && record.tool != "web-input")
            .cloned()
            .collect();
        sort_records(&mut records);
        if records.len() > FULL_LOG_LIMIT {
            records.drain(..records.len() - FULL_LOG_LIMIT);
        }
        records
    }

    fn compact(&mut self) -> io::Result<()> {
        let mut keep = HashSet::new();
        for record in self.operations() {
            keep.insert(record.id);
        }
        for record in self.full_log() {
            keep.insert(record.id);
        }
        let pane_ids: HashSet<_> = self
            .records
            .values()
            .flat_map(|record| record.target.pane_ids.iter().cloned())
            .collect();
        for pane_id in pane_ids {
            for record in self.records_for_pane(&pane_id) {
                keep.insert(record.id);
            }
        }
        let temporary = self.path.with_extension("jsonl.tmp");
        let backup = self.path.with_extension("jsonl.bak");
        remove_file_if_exists(&temporary)?;
        remove_file_if_exists(&backup)?;
        let mut file = File::create(&temporary)?;
        for record in self
            .sorted_records()
            .into_iter()
            .filter(|record| keep.contains(&record.id))
        {
            serde_json::to_writer(&mut file, &record)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        drop(file);
        if self.path.exists() {
            fs::rename(&self.path, &backup)?;
        }
        if let Err(error) = fs::rename(&temporary, &self.path) {
            if !self.path.exists() && backup.exists() {
                let _ = fs::rename(&backup, &self.path);
            }
            return Err(error);
        }
        self.records.retain(|id, _| keep.contains(id));
        remove_file_if_exists(&backup)
    }
}

fn sort_records(records: &mut [ActionRecord]) {
    records.sort_by(|left, right| {
        (left.requested_at_ms, left.updated_at_ms, left.id.as_str()).cmp(&(
            right.requested_at_ms,
            right.updated_at_ms,
            right.id.as_str(),
        ))
    });
}

fn recover_compaction(path: &std::path::Path) -> io::Result<()> {
    let temporary = path.with_extension("jsonl.tmp");
    let backup = path.with_extension("jsonl.bak");
    if path.exists() {
        remove_file_if_exists(&temporary)?;
        remove_file_if_exists(&backup)?;
        return Ok(());
    }
    if temporary.exists() && store_is_complete(&temporary)? {
        fs::rename(&temporary, path)?;
        remove_file_if_exists(&backup)?;
        return Ok(());
    }
    if backup.exists() && store_is_complete(&backup)? {
        fs::rename(&backup, path)?;
        remove_file_if_exists(&temporary)?;
        return Ok(());
    }
    if temporary.exists() || backup.exists() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no complete event store remains after interrupted compaction",
        ));
    }
    Ok(())
}

fn store_is_complete(path: &std::path::Path) -> io::Result<bool> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok(true);
    }
    if !bytes.ends_with(b"\n") {
        return Ok(false);
    }
    for line in bytes[..bytes.len() - 1].split(|byte| *byte == b'\n') {
        let Ok(record) = serde_json::from_slice::<ActionRecord>(line) else {
            return Ok(false);
        };
        if record.schema_version != ACTION_SCHEMA_VERSION {
            return Ok(false);
        }
    }
    Ok(true)
}

fn remove_file_if_exists(path: &std::path::Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn repair_partial_tail(path: &std::path::Path) -> io::Result<()> {
    let mut file = match OpenOptions::new().read(true).append(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    if last[0] != b'\n' {
        file.write_all(b"\n")?;
        file.flush()?;
    }
    Ok(())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
