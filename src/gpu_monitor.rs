//! One lightweight GPU-idle watcher owned by one MCP process.

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{broadcast, watch, Mutex};
use uuid::Uuid;

use crate::tmux;

const MAX_HISTORY: usize = 32;
const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
const MAX_PROTOCOL_LINE_BYTES: usize = 4 * 1024;
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(90);
const STOP_GRACE: Duration = Duration::from_secs(5);
const REMOTE_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LIFETIME_SECONDS: u64 = 24 * 60 * 60;

const REMOTE_WATCH_SCRIPT: &str = r#"set -eu
id=$1
poll=$2
max_samples=$3
trap 'exit 0' HUP INT TERM
printf 'TMUX_MCP_GPU\tREADY\t%s\t%s\n' "$id" "$$"
inventory=$(nvidia-smi --query-gpu=index,uuid --format=csv,noheader,nounits) || {
  printf 'TMUX_MCP_GPU\tERROR\tnvidia-smi inventory failed\n'
  exit 20
}
inventory=$(printf '%s\n' "$inventory" | awk -F',' 'NF >= 2 { gsub(/[[:space:]]/, "", $1); gsub(/[[:space:]]/, "", $2); if (out != "") out=out ";"; out=out $1 "=" $2 } END { print out }')
printf 'TMUX_MCP_GPU\tINVENTORY\t%s\n' "$inventory"
i=0
while [ "$i" -lt "$max_samples" ]; do
  active=$(nvidia-smi --query-compute-apps=gpu_uuid --format=csv,noheader,nounits) || {
    printf 'TMUX_MCP_GPU\tERROR\tnvidia-smi process query failed\n'
    exit 21
  }
  active=$(printf '%s\n' "$active" | awk 'NF { gsub(/[[:space:]]/, ""); if (out != "") out=out ","; out=out $0 } END { print out }')
  printf 'TMUX_MCP_GPU\tSAMPLE\t%s\t%s\n' "$(date +%s)" "$active"
  i=$((i + 1))
  sleep "$poll"
done
printf 'TMUX_MCP_GPU\tEXPIRED\t%s\n' "$(date +%s)"
"#;

const REMOTE_SIGNAL_SCRIPT: &str = r#"set -eu
pid=$1
token=$2
signal=$3
file=/proc/$pid/environ
[ -r "$file" ] || exit 3
tr '\000' '\n' < "$file" | grep -Fqx "TMUX_MCP_GPU_WATCH_ID=$token" || exit 4
kill "-$signal" "$pid"
"#;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartGpuWatchInput {
    /// GPU index or UUID. Omit to watch all physical GPUs.
    pub gpu: Option<String>,
    /// Required continuous idle time. Defaults to 300 seconds.
    #[serde(default = "default_idle_seconds")]
    pub idle_seconds: u64,
    /// Remote sample interval. Defaults to 15 seconds.
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
}

const fn default_idle_seconds() -> u64 {
    300
}

const fn default_poll_seconds() -> u64 {
    15
}

impl Default for StartGpuWatchInput {
    fn default() -> Self {
        Self {
            gpu: None,
            idle_seconds: default_idle_seconds(),
            poll_seconds: default_poll_seconds(),
        }
    }
}

impl StartGpuWatchInput {
    fn validate(&self) -> Result<(), String> {
        if !(60..=86_400).contains(&self.idle_seconds) {
            return Err("idleSeconds must be between 60 and 86400".to_string());
        }
        if !(5..=60).contains(&self.poll_seconds) {
            return Err("pollSeconds must be between 5 and 60".to_string());
        }
        if let Some(gpu) = &self.gpu {
            let valid_index = !gpu.is_empty() && gpu.chars().all(|ch| ch.is_ascii_digit());
            let valid_uuid = gpu.starts_with("GPU-")
                && gpu.len() > 4
                && gpu[4..]
                    .chars()
                    .all(|ch| ch.is_ascii_hexdigit() || ch == '-');
            if !valid_index && !valid_uuid {
                return Err("gpu must be a numeric index or GPU UUID".to_string());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GpuWatchStatus {
    Starting,
    Watching,
    Stopping,
    Triggered,
    Failed,
    Expired,
    Stopped,
    StopUnconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationStatus {
    NotSent,
    TransportSent,
    SendFailed,
    Observed,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GpuWatchSnapshot {
    pub monitor_id: String,
    pub status: GpuWatchStatus,
    pub requested_gpu: Option<String>,
    pub target_gpus: Vec<String>,
    pub idle_gpus: Vec<String>,
    pub idle_seconds: u64,
    pub poll_seconds: u64,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub last_sample_at_ms: Option<u64>,
    pub completed_at_ms: Option<u64>,
    pub local_ssh_pid: Option<u32>,
    pub remote_pid: Option<u32>,
    pub notification_status: NotificationStatus,
    pub reason: Option<String>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GpuWatchQuery {
    pub active: Option<GpuWatchSnapshot>,
    pub recent: Vec<GpuWatchSnapshot>,
}

#[derive(Debug, Clone)]
pub struct GpuMonitorEvent {
    pub snapshot: GpuWatchSnapshot,
}

struct ActiveMonitor {
    snapshot: GpuWatchSnapshot,
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

#[derive(Default)]
struct MonitorState {
    active: Option<ActiveMonitor>,
    history: VecDeque<GpuWatchSnapshot>,
}

#[derive(Clone)]
pub struct GpuMonitorManager {
    state: Arc<Mutex<MonitorState>>,
    events: broadcast::Sender<GpuMonitorEvent>,
}

impl Default for GpuMonitorManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuMonitorManager {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(16);
        Self {
            state: Arc::new(Mutex::new(MonitorState::default())),
            events,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GpuMonitorEvent> {
        self.events.subscribe()
    }

    #[cfg(test)]
    pub fn emit_for_test(&self, snapshot: GpuWatchSnapshot) {
        let _ = self.events.send(GpuMonitorEvent { snapshot });
    }

    pub async fn start(&self, input: StartGpuWatchInput) -> Result<GpuWatchSnapshot, String> {
        input.validate()?;
        if !tmux::ssh_enabled().map_err(|error| error.to_string())? {
            return Err("GPU monitoring requires --ssh or TMUX_MCP_SSH".to_string());
        }

        let monitor_id = format!("gpu-watch-{}", Uuid::new_v4());
        let token = Uuid::new_v4().to_string();
        let snapshot = GpuWatchSnapshot {
            monitor_id: monitor_id.clone(),
            status: GpuWatchStatus::Starting,
            requested_gpu: input.gpu.clone(),
            target_gpus: Vec::new(),
            idle_gpus: Vec::new(),
            idle_seconds: input.idle_seconds,
            poll_seconds: input.poll_seconds,
            created_at_ms: unix_time_ms(),
            started_at_ms: None,
            last_sample_at_ms: None,
            completed_at_ms: None,
            local_ssh_pid: None,
            remote_pid: None,
            notification_status: NotificationStatus::NotSent,
            reason: None,
            diagnostic: None,
        };
        let (cancel, cancel_rx) = watch::channel(false);
        let (done_tx, done) = watch::channel(false);

        {
            let mut state = self.state.lock().await;
            if let Some(active) = &state.active {
                return Err(format!(
                    "GPU watcher {} is already active",
                    active.snapshot.monitor_id
                ));
            }
            state.active = Some(ActiveMonitor {
                snapshot: snapshot.clone(),
                cancel,
                done,
            });
        }

        let manager = self.clone();
        tokio::spawn(async move {
            manager
                .supervise(monitor_id, token, input, cancel_rx, done_tx)
                .await;
        });
        Ok(snapshot)
    }

    pub async fn query(&self, monitor_id: Option<&str>) -> Result<GpuWatchQuery, String> {
        let mut state = self.state.lock().await;
        if let Some(id) = monitor_id {
            if let Some(active) = state.active.as_mut() {
                if active.snapshot.monitor_id == id {
                    mark_observed(&mut active.snapshot);
                    return Ok(GpuWatchQuery {
                        active: Some(active.snapshot.clone()),
                        recent: Vec::new(),
                    });
                }
            }
            if let Some(snapshot) = state
                .history
                .iter_mut()
                .find(|snapshot| snapshot.monitor_id == id)
            {
                mark_observed(snapshot);
                return Ok(GpuWatchQuery {
                    active: None,
                    recent: vec![snapshot.clone()],
                });
            }
            return Err(format!("GPU watcher {id} not found"));
        }

        if let Some(active) = state.active.as_mut() {
            mark_observed(&mut active.snapshot);
        }
        if let Some(snapshot) = state.history.back_mut() {
            mark_observed(snapshot);
        }
        Ok(GpuWatchQuery {
            active: state.active.as_ref().map(|active| active.snapshot.clone()),
            recent: state.history.iter().rev().cloned().collect(),
        })
    }

    pub async fn stop(&self, monitor_id: &str) -> Result<GpuWatchSnapshot, String> {
        let mut done = {
            let mut state = self.state.lock().await;
            if let Some(active) = state.active.as_mut() {
                if active.snapshot.monitor_id == monitor_id {
                    active.snapshot.status = GpuWatchStatus::Stopping;
                    let _ = active.cancel.send(true);
                    active.done.clone()
                } else if let Some(snapshot) = state
                    .history
                    .iter()
                    .find(|snapshot| snapshot.monitor_id == monitor_id)
                {
                    return Ok(snapshot.clone());
                } else {
                    return Err(format!("GPU watcher {monitor_id} not found"));
                }
            } else if let Some(snapshot) = state
                .history
                .iter()
                .find(|snapshot| snapshot.monitor_id == monitor_id)
            {
                return Ok(snapshot.clone());
            } else {
                return Err(format!("GPU watcher {monitor_id} not found"));
            }
        };

        tokio::time::timeout(Duration::from_secs(15), async {
            while !*done.borrow() {
                if done.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .map_err(|_| format!("GPU watcher {monitor_id} stop timed out"))?;

        self.query(Some(monitor_id))
            .await?
            .recent
            .into_iter()
            .next()
            .ok_or_else(|| format!("GPU watcher {monitor_id} terminal state missing"))
    }

    pub async fn shutdown(&self) {
        let id = self
            .state
            .lock()
            .await
            .active
            .as_ref()
            .map(|active| active.snapshot.monitor_id.clone());
        if let Some(id) = id {
            let _ = self.stop(&id).await;
        }
    }

    pub async fn mark_notification(&self, monitor_id: &str, sent: bool) {
        let mut state = self.state.lock().await;
        if let Some(snapshot) = state
            .history
            .iter_mut()
            .find(|snapshot| snapshot.monitor_id == monitor_id)
        {
            snapshot.notification_status = if sent {
                NotificationStatus::TransportSent
            } else {
                NotificationStatus::SendFailed
            };
        }
    }

    async fn supervise(
        &self,
        monitor_id: String,
        token: String,
        input: StartGpuWatchInput,
        mut cancel: watch::Receiver<bool>,
        done: watch::Sender<bool>,
    ) {
        let result = self
            .run_remote(&monitor_id, &token, &input, &mut cancel)
            .await;
        let (status, idle_gpus, reason, diagnostic) = match result {
            Ok(WatchOutcome::Triggered(gpus)) => (
                GpuWatchStatus::Triggered,
                gpus,
                Some("no_compute_process".to_string()),
                None,
            ),
            Ok(WatchOutcome::Stopped(true)) => (GpuWatchStatus::Stopped, Vec::new(), None, None),
            Ok(WatchOutcome::Stopped(false)) => (
                GpuWatchStatus::StopUnconfirmed,
                Vec::new(),
                Some("remote termination unconfirmed".to_string()),
                None,
            ),
            Ok(WatchOutcome::Expired) => (
                GpuWatchStatus::Expired,
                Vec::new(),
                Some("24-hour watcher limit reached".to_string()),
                None,
            ),
            Err(error) => (
                GpuWatchStatus::Failed,
                Vec::new(),
                Some(error.clone()),
                Some(error),
            ),
        };
        self.finish(&monitor_id, status, idle_gpus, reason, diagnostic)
            .await;
        let _ = done.send(true);
    }

    async fn run_remote(
        &self,
        monitor_id: &str,
        token: &str,
        input: &StartGpuWatchInput,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<WatchOutcome, String> {
        let max_samples = (MAX_LIFETIME_SECONDS + input.poll_seconds - 1) / input.poll_seconds;
        let remote = format!(
            "env {} /bin/sh -c {} -- {} {} {}",
            tmux::quote_remote_arg(&format!("TMUX_MCP_GPU_WATCH_ID={token}")),
            tmux::quote_remote_arg(REMOTE_WATCH_SCRIPT),
            tmux::quote_remote_arg(monitor_id),
            input.poll_seconds,
            max_samples
        );
        let mut command = tmux::build_ssh_remote_process(remote).map_err(|e| e.to_string())?;
        command
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to spawn GPU watcher SSH: {error}"))?;
        let local_pid = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "GPU watcher stdout unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "GPU watcher stderr unavailable".to_string())?;
        let stderr_task = tokio::spawn(read_bounded(stderr, MAX_DIAGNOSTIC_BYTES));
        self.update(monitor_id, |snapshot| snapshot.local_ssh_pid = local_pid)
            .await;

        let mut lines = BufReader::new(stdout).lines();
        let ready_deadline = tokio::time::sleep(READY_TIMEOUT);
        tokio::pin!(ready_deadline);
        let mut remote_pid = None;
        let mut targets = Vec::new();
        let mut known_gpus = HashSet::new();
        let mut idle_since: HashMap<String, Instant> = HashMap::new();
        let mut heartbeat_deadline = Box::pin(tokio::time::sleep(HEARTBEAT_TIMEOUT));
        let mut ready = false;

        loop {
            tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        let confirmed = terminate_remote(&mut child, remote_pid, token).await;
                        stderr_task.abort();
                        return Ok(WatchOutcome::Stopped(confirmed));
                    }
                }
                _ = &mut ready_deadline, if !ready => {
                    let confirmed = terminate_remote(&mut child, remote_pid, token).await;
                    stderr_task.abort();
                    return Err(if confirmed {
                        "GPU watcher READY timed out".to_string()
                    } else {
                        "GPU watcher READY timed out; remote termination unconfirmed".to_string()
                    });
                }
                _ = &mut heartbeat_deadline, if ready => {
                    let confirmed = terminate_remote(&mut child, remote_pid, token).await;
                    stderr_task.abort();
                    return Err(if confirmed {
                        "GPU watcher sample timed out".to_string()
                    } else {
                        "GPU watcher sample timed out; remote termination unconfirmed".to_string()
                    });
                }
                line = lines.next_line() => {
                    let line = line.map_err(|error| format!("GPU watcher output: {error}"))?;
                    let Some(line) = line else {
                        let status = child.wait().await.map_err(|error| format!("GPU watcher wait: {error}"))?;
                        let diagnostic = stderr_task.await.ok().and_then(Result::ok).unwrap_or_default();
                        return Err(format!("GPU watcher exited with {status}: {diagnostic}"));
                    };
                    if line.len() > MAX_PROTOCOL_LINE_BYTES {
                        let _ = terminate_remote(&mut child, remote_pid, token).await;
                        stderr_task.abort();
                        return Err("GPU watcher protocol line exceeds 4096 bytes".to_string());
                    }
                    let message = parse_protocol_line(&line)?;
                    match message {
                        ProtocolMessage::Ready { id, pid } => {
                            if id != monitor_id {
                                let _ = terminate_remote(&mut child, Some(pid), token).await;
                                stderr_task.abort();
                                return Err("GPU watcher READY id mismatch".to_string());
                            }
                            remote_pid = Some(pid);
                        }
                        ProtocolMessage::Inventory(inventory) => {
                            if remote_pid.is_none() {
                                return Err("GPU watcher INVENTORY arrived before READY".to_string());
                            }
                            targets = resolve_targets(input.gpu.as_deref(), &inventory)?;
                            known_gpus = inventory
                                .iter()
                                .map(|(_, uuid)| uuid.clone())
                                .collect();
                            ready = true;
                            heartbeat_deadline.as_mut().reset(tokio::time::Instant::now() + HEARTBEAT_TIMEOUT);
                            self.update(monitor_id, |snapshot| {
                                snapshot.status = GpuWatchStatus::Watching;
                                snapshot.remote_pid = remote_pid;
                                snapshot.target_gpus = targets.clone();
                                snapshot.started_at_ms = Some(unix_time_ms());
                            }).await;
                        }
                        ProtocolMessage::Sample { active } => {
                            if !ready {
                                return Err("GPU watcher SAMPLE arrived before INVENTORY".to_string());
                            }
                            heartbeat_deadline.as_mut().reset(tokio::time::Instant::now() + HEARTBEAT_TIMEOUT);
                            if active.iter().any(|uuid| !known_gpus.contains(uuid)) {
                                let _ = terminate_remote(&mut child, remote_pid, token).await;
                                stderr_task.abort();
                                return Err("GPU process UUID does not map to physical inventory".to_string());
                            }
                            let now = Instant::now();
                            let triggered = update_idle_state(
                                &targets,
                                &active,
                                &mut idle_since,
                                now,
                                Duration::from_secs(input.idle_seconds),
                            );
                            self.update(monitor_id, |snapshot| {
                                snapshot.last_sample_at_ms = Some(unix_time_ms());
                            }).await;
                            if !triggered.is_empty() {
                                let _ = terminate_remote(&mut child, remote_pid, token).await;
                                stderr_task.abort();
                                return Ok(WatchOutcome::Triggered(triggered));
                            }
                        }
                        ProtocolMessage::Expired => {
                            let _ = child.wait().await;
                            stderr_task.abort();
                            return Ok(WatchOutcome::Expired);
                        }
                        ProtocolMessage::Error(reason) => {
                            let _ = child.wait().await;
                            let diagnostic = stderr_task.await.ok().and_then(Result::ok).unwrap_or_default();
                            return Err(if diagnostic.is_empty() { reason } else { format!("{reason}: {diagnostic}") });
                        }
                    }
                }
            }
        }
    }

    async fn update(&self, monitor_id: &str, apply: impl FnOnce(&mut GpuWatchSnapshot)) {
        let mut state = self.state.lock().await;
        if let Some(active) = state.active.as_mut() {
            if active.snapshot.monitor_id == monitor_id {
                apply(&mut active.snapshot);
            }
        }
    }

    async fn finish(
        &self,
        monitor_id: &str,
        status: GpuWatchStatus,
        idle_gpus: Vec<String>,
        reason: Option<String>,
        diagnostic: Option<String>,
    ) {
        let snapshot = {
            let mut state = self.state.lock().await;
            let Some(mut active) = state.active.take() else {
                return;
            };
            if active.snapshot.monitor_id != monitor_id {
                state.active = Some(active);
                return;
            }
            active.snapshot.status = status;
            active.snapshot.idle_gpus = idle_gpus;
            active.snapshot.reason = reason;
            active.snapshot.diagnostic = diagnostic;
            active.snapshot.completed_at_ms = Some(unix_time_ms());
            let snapshot = active.snapshot;
            state.history.push_back(snapshot.clone());
            while state.history.len() > MAX_HISTORY {
                state.history.pop_front();
            }
            snapshot
        };
        let _ = self.events.send(GpuMonitorEvent { snapshot });
    }
}

fn mark_observed(snapshot: &mut GpuWatchSnapshot) {
    if snapshot.notification_status == NotificationStatus::TransportSent {
        snapshot.notification_status = NotificationStatus::Observed;
    }
}

enum WatchOutcome {
    Triggered(Vec<String>),
    Stopped(bool),
    Expired,
}

enum ProtocolMessage {
    Ready { id: String, pid: u32 },
    Inventory(Vec<(String, String)>),
    Sample { active: HashSet<String> },
    Error(String),
    Expired,
}

fn parse_protocol_line(line: &str) -> Result<ProtocolMessage, String> {
    let mut fields = line.split('\t');
    if fields.next() != Some("TMUX_MCP_GPU") {
        return Err("unexpected GPU watcher output".to_string());
    }
    match fields.next() {
        Some("READY") => {
            let id = fields
                .next()
                .ok_or_else(|| "GPU watcher READY missing id".to_string())?;
            let pid = fields
                .next()
                .ok_or_else(|| "GPU watcher READY missing pid".to_string())?
                .parse::<u32>()
                .map_err(|_| "GPU watcher READY has invalid pid".to_string())?;
            Ok(ProtocolMessage::Ready {
                id: id.to_string(),
                pid,
            })
        }
        Some("INVENTORY") => {
            let value = fields.next().unwrap_or_default();
            let mut inventory = Vec::new();
            for entry in value.split(';').filter(|entry| !entry.is_empty()) {
                let (index, uuid) = entry
                    .split_once('=')
                    .ok_or_else(|| "GPU watcher INVENTORY is malformed".to_string())?;
                inventory.push((index.to_string(), uuid.to_string()));
            }
            if inventory.is_empty() {
                return Err("GPU watcher found no GPUs".to_string());
            }
            Ok(ProtocolMessage::Inventory(inventory))
        }
        Some("SAMPLE") => {
            let _epoch = fields
                .next()
                .ok_or_else(|| "GPU watcher SAMPLE missing time".to_string())?;
            let active = fields
                .next()
                .unwrap_or_default()
                .split(',')
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect();
            Ok(ProtocolMessage::Sample { active })
        }
        Some("ERROR") => Ok(ProtocolMessage::Error(
            fields.next().unwrap_or("remote watcher failed").to_string(),
        )),
        Some("EXPIRED") => Ok(ProtocolMessage::Expired),
        _ => Err("unknown GPU watcher protocol message".to_string()),
    }
}

fn resolve_targets(
    requested: Option<&str>,
    inventory: &[(String, String)],
) -> Result<Vec<String>, String> {
    match requested {
        None => Ok(inventory.iter().map(|(_, uuid)| uuid.clone()).collect()),
        Some(value) if value.chars().all(|ch| ch.is_ascii_digit()) => inventory
            .iter()
            .find(|(index, _)| index == value)
            .map(|(_, uuid)| vec![uuid.clone()])
            .ok_or_else(|| format!("GPU index {value} not found")),
        Some(value) => inventory
            .iter()
            .find(|(_, uuid)| uuid == value)
            .map(|(_, uuid)| vec![uuid.clone()])
            .ok_or_else(|| format!("GPU UUID {value} not found")),
    }
}

fn update_idle_state(
    targets: &[String],
    active: &HashSet<String>,
    idle_since: &mut HashMap<String, Instant>,
    now: Instant,
    required_idle: Duration,
) -> Vec<String> {
    let mut triggered = Vec::new();
    for gpu in targets {
        if active.contains(gpu) {
            idle_since.remove(gpu);
        } else {
            let since = idle_since.entry(gpu.clone()).or_insert(now);
            if now.duration_since(*since) >= required_idle {
                triggered.push(gpu.clone());
            }
        }
    }
    triggered
}

async fn terminate_remote(child: &mut Child, remote_pid: Option<u32>, token: &str) -> bool {
    let Some(pid) = remote_pid else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return false;
    };
    match signal_remote(pid, token, "TERM").await {
        SignalResult::Gone => {
            if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_ok() {
                true
            } else {
                let _ = child.kill().await;
                let _ = child.wait().await;
                false
            }
        }
        SignalResult::Sent => {
            if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_ok() {
                return true;
            }
            if !matches!(
                signal_remote(pid, token, "KILL").await,
                SignalResult::Sent | SignalResult::Gone
            ) {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return false;
            }
            let confirmed = tokio::time::timeout(STOP_GRACE, child.wait()).await.is_ok();
            if !confirmed {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            confirmed
        }
        SignalResult::Mismatch | SignalResult::Failed => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            false
        }
    }
}

enum SignalResult {
    Sent,
    Gone,
    Mismatch,
    Failed,
}

async fn signal_remote(pid: u32, token: &str, signal: &str) -> SignalResult {
    let remote = format!(
        "/bin/sh -c {} -- {} {} {}",
        tmux::quote_remote_arg(REMOTE_SIGNAL_SCRIPT),
        pid,
        tmux::quote_remote_arg(token),
        signal
    );
    let Ok(mut command) = tmux::build_ssh_remote_process(remote) else {
        return SignalResult::Failed;
    };
    command
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match tokio::time::timeout(REMOTE_CONTROL_TIMEOUT, command.status()).await {
        Ok(Ok(status)) if status.success() => SignalResult::Sent,
        Ok(Ok(status)) if status.code() == Some(3) => SignalResult::Gone,
        Ok(Ok(status)) if status.code() == Some(4) => SignalResult::Mismatch,
        _ => SignalResult::Failed,
    }
}

async fn read_bounded<R: AsyncRead + Unpin>(
    reader: R,
    max_bytes: usize,
) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes as u64)
        .read_to_end(&mut bytes)
        .await?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_small_fixed_input_surface() {
        assert!(StartGpuWatchInput::default().validate().is_ok());
        assert!(StartGpuWatchInput {
            gpu: Some("GPU-acde-1234".to_string()),
            ..Default::default()
        }
        .validate()
        .is_ok());
        assert!(StartGpuWatchInput {
            gpu: Some("0; rm -rf /".to_string()),
            ..Default::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn parses_protocol_and_resolves_stable_uuid() {
        let ProtocolMessage::Ready { id, pid } =
            parse_protocol_line("TMUX_MCP_GPU\tREADY\tgpu-watch-x\t42").unwrap()
        else {
            panic!("ready");
        };
        assert_eq!(id, "gpu-watch-x");
        assert_eq!(pid, 42);

        let ProtocolMessage::Inventory(inventory) =
            parse_protocol_line("TMUX_MCP_GPU\tINVENTORY\t0=GPU-aaaa;1=GPU-bbbb").unwrap()
        else {
            panic!("inventory");
        };
        assert_eq!(
            resolve_targets(Some("1"), &inventory).unwrap(),
            ["GPU-bbbb"]
        );
        assert_eq!(resolve_targets(None, &inventory).unwrap().len(), 2);
    }

    #[test]
    fn malformed_or_unknown_protocol_is_rejected() {
        assert!(parse_protocol_line("noise").is_err());
        assert!(parse_protocol_line("TMUX_MCP_GPU\tREADY\tid\tbad").is_err());
        assert!(parse_protocol_line("TMUX_MCP_GPU\tINVENTORY\t").is_err());
    }

    #[test]
    fn idle_timer_is_per_gpu_and_activity_resets_it() {
        let start = Instant::now();
        let targets = vec!["GPU-a".to_string(), "GPU-b".to_string()];
        let mut idle_since = HashMap::new();
        let mut active = HashSet::from(["GPU-a".to_string()]);

        assert!(update_idle_state(
            &targets,
            &active,
            &mut idle_since,
            start,
            Duration::from_secs(300),
        )
        .is_empty());
        assert_eq!(
            update_idle_state(
                &targets,
                &active,
                &mut idle_since,
                start + Duration::from_secs(300),
                Duration::from_secs(300),
            ),
            ["GPU-b"]
        );

        active.insert("GPU-b".to_string());
        assert!(update_idle_state(
            &targets,
            &active,
            &mut idle_since,
            start + Duration::from_secs(301),
            Duration::from_secs(300),
        )
        .is_empty());
        active.remove("GPU-b");
        assert!(update_idle_state(
            &targets,
            &active,
            &mut idle_since,
            start + Duration::from_secs(600),
            Duration::from_secs(300),
        )
        .is_empty());
    }

    #[tokio::test]
    async fn stop_requires_exact_id_and_is_idempotent_after_terminal() {
        let manager = GpuMonitorManager::new();
        let snapshot = GpuWatchSnapshot {
            monitor_id: "gpu-watch-test".to_string(),
            status: GpuWatchStatus::Watching,
            requested_gpu: None,
            target_gpus: vec!["GPU-a".to_string()],
            idle_gpus: Vec::new(),
            idle_seconds: 300,
            poll_seconds: 15,
            created_at_ms: 1,
            started_at_ms: Some(2),
            last_sample_at_ms: Some(3),
            completed_at_ms: None,
            local_ssh_pid: Some(10),
            remote_pid: Some(20),
            notification_status: NotificationStatus::NotSent,
            reason: None,
            diagnostic: None,
        };
        let (cancel, mut cancel_rx) = watch::channel(false);
        let (done_tx, done) = watch::channel(false);
        manager.state.lock().await.active = Some(ActiveMonitor {
            snapshot,
            cancel,
            done,
        });

        assert!(manager.stop("wrong-id").await.is_err());
        let finishing = manager.clone();
        tokio::spawn(async move {
            cancel_rx.changed().await.unwrap();
            finishing
                .finish(
                    "gpu-watch-test",
                    GpuWatchStatus::Stopped,
                    Vec::new(),
                    None,
                    None,
                )
                .await;
            let _ = done_tx.send(true);
        });

        let stopped = manager.stop("gpu-watch-test").await.unwrap();
        assert_eq!(stopped.status, GpuWatchStatus::Stopped);
        let repeated = manager.stop("gpu-watch-test").await.unwrap();
        assert_eq!(repeated.status, GpuWatchStatus::Stopped);
    }
}
