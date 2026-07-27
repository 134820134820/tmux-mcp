use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::commands::CommandTracker;
use crate::types::{CommandSnapshot, CommandStatus};

pub const ACTION_SCHEMA_VERSION: u32 = 1;
pub(crate) const AGENT_RECORD_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Command,
    Input,
    Operation,
}

impl ActionKind {
    fn for_tool(tool: &str) -> Self {
        match tool {
            "execute-command" => Self::Command,
            "send-keys" | "send-hex" | "paste-text" | "web-input" => Self::Input,
            name if name.starts_with("send-") => Self::Input,
            _ => Self::Operation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Requested,
    WaitingApproval,
    Approved,
    Running,
    Completed,
    Failed,
    Rejected,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateMode {
    Off,
    Tools,
    Approval,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationResponse {
    pub decision: GateDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiPause {
    pub pane_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionTarget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pane_ids: Vec<String>,
}

impl ActionTarget {
    fn from_arguments(arguments: &Value) -> Self {
        let string = |name: &str| {
            arguments
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let mut pane_ids = Vec::new();
        for name in ["paneId", "sourcePaneId", "targetPaneId"] {
            if let Some(pane_id) = string(name) {
                if !pane_ids.contains(&pane_id) {
                    pane_ids.push(pane_id);
                }
            }
        }
        Self {
            socket: string("socket"),
            session_id: string("sessionId"),
            window_id: string("windowId"),
            pane_ids,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRecord {
    pub schema_version: u32,
    pub id: String,
    pub source: String,
    pub tool: String,
    pub kind: ActionKind,
    #[serde(default)]
    pub read_only: bool,
    pub target: ActionTarget,
    pub arguments: Value,
    pub status: ActionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_snapshot: Option<CommandSnapshot>,
    pub requested_at_ms: u64,
    pub updated_at_ms: u64,
}

impl ActionRecord {
    pub fn new(source: impl Into<String>, tool: impl Into<String>, arguments: Value) -> Self {
        let tool = tool.into();
        let now = unix_time_ms();
        Self {
            schema_version: ACTION_SCHEMA_VERSION,
            id: Uuid::new_v4().to_string(),
            source: source.into(),
            kind: ActionKind::for_tool(&tool),
            read_only: false,
            target: ActionTarget::from_arguments(&arguments),
            tool,
            arguments,
            status: ActionStatus::Requested,
            result: None,
            command_snapshot: None,
            requested_at_ms: now,
            updated_at_ms: now,
        }
    }

    pub fn mark_running(&mut self) {
        self.status = ActionStatus::Running;
        self.touch();
    }

    pub fn mark_waiting_approval(&mut self) {
        self.status = ActionStatus::WaitingApproval;
        self.touch();
    }

    pub fn mark_approved(&mut self) {
        self.status = ActionStatus::Approved;
        self.touch();
    }

    pub fn mark_rejected(&mut self) {
        self.status = ActionStatus::Rejected;
        self.touch();
    }

    pub fn mark_incomplete(&mut self) {
        self.status = ActionStatus::Incomplete;
        self.touch();
    }

    pub fn mark_completed(&mut self, result: Option<Value>) {
        self.status = ActionStatus::Completed;
        self.result = result;
        self.touch();
    }

    pub fn mark_failed(&mut self, result: Option<Value>) {
        self.status = ActionStatus::Failed;
        self.result = result;
        self.touch();
    }

    pub fn mark_command(&mut self, snapshot: CommandSnapshot) {
        if self.arguments.get("command").and_then(Value::as_str) == Some(snapshot.command.as_str())
        {
            self.arguments
                .as_object_mut()
                .expect("command arguments are an object")
                .remove("command");
        }
        self.status = match snapshot.status {
            CommandStatus::Completed => ActionStatus::Completed,
            CommandStatus::Failed | CommandStatus::Cancelled | CommandStatus::TrackingError => {
                ActionStatus::Failed
            }
            CommandStatus::Queued | CommandStatus::Running => ActionStatus::Running,
        };
        self.command_snapshot = Some(snapshot);
        self.touch();
        let encoded_len = serde_json::to_vec(&self).map_or(0, |encoded| encoded.len());
        let excess = encoded_len.saturating_sub(AGENT_RECORD_MAX_BYTES - 1);
        if excess == 0 {
            return;
        }
        if let Some(snapshot) = &mut self.command_snapshot {
            if let Some(output) = &mut snapshot.output {
                let mut keep = output.len().saturating_sub(excess);
                while !output.is_char_boundary(keep) {
                    keep -= 1;
                }
                output.truncate(keep);
                snapshot.output_truncated = true;
            }
        }
    }

    fn touch(&mut self) {
        self.updated_at_ms = unix_time_ms();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePaths {
    pub directory: PathBuf,
    pub events: PathBuf,
    pub gate: PathBuf,
    pub ai_pause: PathBuf,
    pub token: PathBuf,
}

#[derive(Clone)]
pub struct ControlClient {
    base_url: reqwest::Url,
    source: Arc<str>,
    paths: StatePaths,
    http: reqwest::Client,
    commands: Arc<tokio::sync::Mutex<HashMap<String, ActionRecord>>>,
}

impl ControlClient {
    pub fn new(
        base_url: &str,
        source: impl Into<String>,
        paths: StatePaths,
    ) -> Result<Self, String> {
        let base_url = validate_web_url(base_url)?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(500))
            .no_proxy()
            .build()
            .map_err(|error| format!("unable to build web control client: {error}"))?;
        Ok(Self {
            base_url,
            source: Arc::from(source.into()),
            paths,
            http,
            commands: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        })
    }

    pub fn action(&self, tool: impl Into<String>, arguments: Value) -> ActionRecord {
        ActionRecord::new(self.source.to_string(), tool, arguments)
    }

    pub async fn authorize(&self, record: &ActionRecord) -> Result<GateDecision, String> {
        let gate_was_enabled = self.paths.gate_enabled();
        let result = self.authorize_request(record, gate_was_enabled).await;
        match result {
            Ok(decision) => Ok(decision),
            Err(error) if gate_was_enabled || self.paths.gate_enabled() => Err(format!(
                "Gate is enabled but the web control service is unavailable: {error}"
            )),
            Err(_) => Ok(GateDecision::Approved),
        }
    }

    pub fn gate_mode(&self) -> GateMode {
        self.paths.gate_mode()
    }

    pub fn ai_pause(&self) -> Result<Option<AiPause>, String> {
        self.paths
            .ai_pause()
            .map_err(|error| format!("AI pause state: {error}"))
    }

    pub fn pause_ai(&self, pane_id: &str) -> Result<(), String> {
        set_ai_pause(&self.paths, pane_id)
            .map_err(|error| format!("unable to persist AI pause: {error}"))
    }

    pub async fn record(&self, record: &ActionRecord) -> Result<(), String> {
        let token =
            load_or_create_token(&self.paths).map_err(|error| format!("control token: {error}"))?;
        let endpoint = self
            .base_url
            .join("/api/agent/record")
            .map_err(|error| format!("record URL: {error}"))?;
        self.http
            .post(endpoint)
            .header("x-tmux-mcp-token", token)
            .timeout(Duration::from_millis(500))
            .json(record)
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub async fn track_command(
        &self,
        command_id: impl Into<String>,
        record: ActionRecord,
    ) -> Result<(), String> {
        self.commands
            .lock()
            .await
            .insert(command_id.into(), record.clone());
        self.record(&record).await
    }

    pub async fn complete_command(&self, snapshot: CommandSnapshot) -> Result<(), String> {
        let Some(mut record) = self
            .commands
            .lock()
            .await
            .get(&snapshot.command_id)
            .cloned()
        else {
            return Ok(());
        };
        record.mark_command(snapshot);
        self.record(&record).await
    }

    // The binary server uses this; the standalone library target has no server module.
    #[allow(dead_code)]
    pub(crate) async fn reconcile_commands(&self, tracker: &CommandTracker) {
        let command_ids = self
            .commands
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for command_id in command_ids {
            let Some(execution) = tracker.get_command(&command_id).await else {
                self.forget_missing_command(&command_id).await;
                continue;
            };
            if !execution.status.is_terminal() {
                continue;
            }
            let presentation_ready = execution.output.is_some() || !execution.output_truncated;
            let recorded = self
                .complete_command(CommandSnapshot::from_execution(&execution, None))
                .await
                .is_ok();
            if recorded && presentation_ready {
                self.forget_command(&command_id).await;
            }
        }
    }

    async fn forget_missing_command(&self, command_id: &str) {
        let Some(mut record) = self.commands.lock().await.remove(command_id) else {
            return;
        };
        if matches!(
            record.status,
            ActionStatus::Requested
                | ActionStatus::WaitingApproval
                | ActionStatus::Approved
                | ActionStatus::Running
        ) {
            record.mark_incomplete();
            let _ = self.record(&record).await;
        }
    }

    pub async fn forget_command(&self, command_id: &str) {
        self.commands.lock().await.remove(command_id);
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) async fn has_test_command(&self, command_id: &str) -> bool {
        self.commands.lock().await.contains_key(command_id)
    }

    async fn authorize_request(
        &self,
        record: &ActionRecord,
        gate_was_enabled: bool,
    ) -> Result<GateDecision, String> {
        let token =
            load_or_create_token(&self.paths).map_err(|error| format!("control token: {error}"))?;
        let endpoint = self
            .base_url
            .join("/api/agent/authorize")
            .map_err(|error| format!("authorization URL: {error}"))?;
        let request = self
            .http
            .post(endpoint)
            .header("x-tmux-mcp-token", token)
            .json(record);
        let request = if gate_was_enabled {
            request
        } else {
            request.timeout(Duration::from_millis(500))
        };
        let response = request
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?;
        response
            .json::<AuthorizationResponse>()
            .await
            .map(|response| response.decision)
            .map_err(|error| error.to_string())
    }
}

impl StatePaths {
    pub fn new(directory: impl AsRef<Path>) -> Self {
        let directory = directory.as_ref().to_path_buf();
        Self {
            events: directory.join("events.jsonl"),
            gate: directory.join("gate.enabled"),
            ai_pause: directory.join("ai-paused.json"),
            token: directory.join("control.token"),
            directory,
        }
    }

    pub fn gate_enabled(&self) -> bool {
        self.gate_mode() != GateMode::Off
    }

    pub fn gate_mode(&self) -> GateMode {
        match fs::read_to_string(&self.gate) {
            Ok(value) if value.trim() == "tools" => GateMode::Tools,
            Ok(_) => GateMode::Approval,
            Err(error) if error.kind() == io::ErrorKind::NotFound => GateMode::Off,
            Err(_) => GateMode::Approval,
        }
    }

    pub fn ai_pause(&self) -> io::Result<Option<AiPause>> {
        match fs::read_to_string(&self.ai_pause) {
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

pub fn set_ai_pause(paths: &StatePaths, pane_id: &str) -> io::Result<()> {
    fs::create_dir_all(&paths.directory)?;
    let value = serde_json::to_vec(&AiPause {
        pane_id: pane_id.to_string(),
    })
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(&paths.ai_pause, value)
}

pub fn clear_ai_pause(paths: &StatePaths) -> io::Result<()> {
    if let Err(error) = fs::remove_file(&paths.ai_pause) {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    Ok(())
}

pub fn set_gate_mode(paths: &StatePaths, mode: GateMode) -> io::Result<()> {
    fs::create_dir_all(&paths.directory)?;
    match mode {
        GateMode::Off => {
            if let Err(error) = fs::remove_file(&paths.gate) {
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
        }
        GateMode::Tools => fs::write(&paths.gate, b"tools\n")?,
        GateMode::Approval => fs::write(&paths.gate, b"approval\n")?,
    }
    Ok(())
}

pub fn default_state_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
        return PathBuf::from(path).join("tmux-mcp");
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path).join("tmux-mcp");
    }
    if let Some(path) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path)
            .join(".local")
            .join("state")
            .join("tmux-mcp");
    }
    std::env::temp_dir().join("tmux-mcp")
}

pub fn load_or_create_token(paths: &StatePaths) -> io::Result<String> {
    fs::create_dir_all(&paths.directory)?;
    match fs::read_to_string(&paths.token) {
        Ok(token) => return validate_token(token),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&paths.token)
    {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            restrict_token_permissions(&paths.token)?;
            Ok(token)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_token(fs::read_to_string(&paths.token)?)
        }
        Err(error) => Err(error),
    }
}

fn validate_token(token: String) -> io::Result<String> {
    let token = token.trim().to_owned();
    if token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(token)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control token must contain exactly 64 hexadecimal characters",
        ))
    }
}

#[cfg(unix)]
fn restrict_token_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_token_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub fn validate_web_url(value: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value).map_err(|error| format!("invalid web URL: {error}"))?;
    if url.scheme() != "http" {
        return Err("web URL must use plain HTTP on loopback".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("web URL must not contain credentials".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "web URL must include a loopback host".to_string())?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false);
    if !loopback {
        return Err("web URL must use a loopback host".into());
    }
    Ok(url)
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
