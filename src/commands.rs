//! Side-channel command execution tracking for agent-driven shell work in panes.
//!
//! `CommandTracker` queues tracked commands per pane, wraps them with optional
//! START/DONE debug markers plus a private tmux exit-code side channel
//! (`set-buffer` + `wait-for`), and commits terminal status only from that
//! channel—not from forgeable pane scrollback.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Deserialize;
use tokio::sync::{broadcast, Notify, RwLock};
use uuid::Uuid;

use crate::errors::{Error, Result};
use crate::tmux;
use crate::types::{command_resource_uri, CommandExecution, CommandStatus, ShellType};

/// Prefix for the start marker, followed by command id.
pub const START_MARKER_PREFIX: &str = "TMUX_MCP_START_";

/// Prefix for the end marker, followed by command id and exit code.
pub const END_MARKER_PREFIX: &str = "TMUX_MCP_DONE_";

const FINAL_OUTPUT_CAPTURE_ATTEMPTS: usize = 3;
const FINAL_OUTPUT_CAPTURE_RETRY_DELAY: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq, Eq)]
enum BracketedOutput {
    /// Both marker boundaries were present in the bounded capture.
    Complete(String),
    /// START was present but DONE was not yet visible.
    Open(String),
    /// The bounded capture did not include START, so output cannot be isolated safely.
    MissingStart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedOutput {
    output: Option<String>,
    /// Capture completeness only; command lifecycle remains side-channel authoritative.
    truncated: bool,
}

impl CapturedOutput {
    fn unavailable() -> Self {
        Self {
            output: None,
            truncated: true,
        }
    }
}

/// Lifecycle event published after a durable tracker commit.
///
/// The MCP server maps these to resource list-changed / resource-updated
/// notifications for subscribed `tmux://command/{id}/result` URIs.
#[derive(Debug, Clone)]
pub struct CommandEvent {
    #[allow(dead_code)]
    pub command_id: String,
    pub resource_uri: String,
    pub kind: CommandEventKind,
    #[allow(dead_code)]
    pub status: CommandStatus,
}

/// Kind of durable commit that produced a [`CommandEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandEventKind {
    /// Command accepted and inserted into the tracker map.
    Created,
    /// Non-terminal field refresh (for example partial pane output).
    Updated,
    /// Status reached a terminal state (completed/failed/cancelled/tracking_error).
    Terminal,
    /// Record removed by retention, pane purge, or abandon window.
    Evicted,
}

/// Capture, retention, and deadline budgets for tracked commands (`[tracking]` in config.toml).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackingConfig {
    /// Lines of scrollback captured when refreshing partial output for running commands.
    #[serde(default = "default_capture_initial_lines")]
    pub capture_initial_lines: u32,
    /// Upper bound on lines used when bracketing START/DONE markers for output only.
    #[serde(default = "default_capture_max_lines")]
    pub capture_max_lines: u32,
    /// Retained for config compatibility; side-channel completion no longer uses capture backoff.
    #[serde(default = "default_capture_backoff_factor")]
    #[allow(dead_code)]
    pub capture_backoff_factor: u32,
    /// How long terminal command records stay queryable before eviction.
    #[serde(default = "default_completed_retention_minutes")]
    pub completed_retention_minutes: u64,
    /// Cap on retained terminal records (oldest completed first when over budget).
    #[serde(default = "default_completed_max_entries")]
    pub completed_max_entries: u32,
    /// Max wait for the side-channel `wait-for` signal before `tracking_error`.
    #[serde(default = "default_tracking_deadline_seconds")]
    pub tracking_deadline_seconds: u64,
}

fn default_capture_initial_lines() -> u32 {
    1000
}

fn default_capture_max_lines() -> u32 {
    16_000
}

fn default_capture_backoff_factor() -> u32 {
    2
}

fn default_completed_retention_minutes() -> u64 {
    240
}

fn default_completed_max_entries() -> u32 {
    1000
}

/// How long the side-channel watcher waits for `wait-for` before tracking_error.
fn default_tracking_deadline_seconds() -> u64 {
    600
}

impl Default for TrackingConfig {
    fn default() -> Self {
        Self {
            capture_initial_lines: default_capture_initial_lines(),
            capture_max_lines: default_capture_max_lines(),
            capture_backoff_factor: default_capture_backoff_factor(),
            completed_retention_minutes: default_completed_retention_minutes(),
            completed_max_entries: default_completed_max_entries(),
            tracking_deadline_seconds: default_tracking_deadline_seconds(),
        }
    }
}

#[derive(Clone)]
struct QueuedLaunch {
    command_id: String,
    pane_id: String,
    command: String,
    delay_ms: Option<u64>,
    socket: Option<String>,
    secret: String,
}

/// In-process registry of queued, running, and recently completed pane commands.
pub struct CommandTracker {
    active_commands: Arc<RwLock<HashMap<String, CommandExecution>>>,
    /// Private side-channel secrets (never exposed on CommandExecution).
    secrets: Arc<RwLock<HashMap<String, String>>>,
    /// pane_key -> command ids waiting to run (tracked only).
    pane_queues: Arc<RwLock<HashMap<String, VecDeque<QueuedLaunch>>>>,
    /// pane_key -> currently running tracked command id.
    pane_running: Arc<RwLock<HashMap<String, String>>>,
    shell_type: ShellType,
    tracking: TrackingConfig,
    events: broadcast::Sender<CommandEvent>,
    notify: Arc<Notify>,
}

impl std::fmt::Debug for CommandTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandTracker")
            .field("shell_type", &self.shell_type)
            .finish_non_exhaustive()
    }
}

impl CommandTracker {
    /// Build a tracker with default capture/retention budgets for `shell_type`.
    pub fn new(shell_type: ShellType) -> Self {
        Self::with_tracking(shell_type, TrackingConfig::default())
    }

    /// Build a tracker with caller-supplied capture/retention budgets.
    pub fn with_tracking(shell_type: ShellType, tracking: TrackingConfig) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            active_commands: Arc::new(RwLock::new(HashMap::new())),
            secrets: Arc::new(RwLock::new(HashMap::new())),
            pane_queues: Arc::new(RwLock::new(HashMap::new())),
            pane_running: Arc::new(RwLock::new(HashMap::new())),
            shell_type,
            tracking,
            events,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Subscribe to command lifecycle events (after durable commits).
    pub fn subscribe_events(&self) -> broadcast::Receiver<CommandEvent> {
        self.events.subscribe()
    }

    fn emit(&self, kind: CommandEventKind, exec: &CommandExecution) {
        let _ = self.events.send(CommandEvent {
            command_id: exec.id.clone(),
            resource_uri: command_resource_uri(&exec.id),
            kind,
            status: exec.status,
        });
        self.notify.notify_waiters();
    }

    fn pane_key(pane_id: &str, socket: Option<&str>) -> String {
        format!("{}|{}", socket.unwrap_or(""), pane_id)
    }

    /// Send a command into a pane. Tracked mode uses side-channel completion.
    ///
    /// Returns a command id for later status/wait. Tracking is disabled for
    /// `raw_mode` or `no_enter`.
    pub async fn execute_command(
        &self,
        pane_id: &str,
        command: &str,
        raw_mode: bool,
        no_enter: bool,
        delay_ms: Option<u64>,
        socket: Option<String>,
    ) -> Result<String> {
        let command_id = Uuid::new_v4().to_string();
        let resolved_socket = tmux::resolve_socket(socket.as_deref());
        let tracking_disabled = raw_mode || no_enter;

        if !tracking_disabled && command.contains(['\n', '\r']) {
            return Err(Error::InvalidArgument {
                message: "tracked commands cannot contain embedded newlines (\\n or \\r)"
                    .to_string(),
            });
        }
        if !tracking_disabled && has_unquoted_shell_comment_marker(command) {
            return Err(Error::InvalidArgument {
                message: "tracked commands cannot contain unquoted shell comment markers (#)"
                    .to_string(),
            });
        }
        if !tracking_disabled && has_unquoted_shell_background_operator(command) {
            return Err(Error::InvalidArgument {
                message: "tracked commands cannot contain unquoted shell background operators (&)"
                    .to_string(),
            });
        }

        let secret = if tracking_disabled {
            None
        } else {
            Some(Uuid::new_v4().simple().to_string())
        };

        let execution = CommandExecution {
            id: command_id.clone(),
            pane_id: pane_id.to_string(),
            socket: resolved_socket.clone(),
            command: command.to_string(),
            status: if tracking_disabled {
                CommandStatus::Running
            } else {
                CommandStatus::Queued
            },
            exit_code: None,
            output: if tracking_disabled {
                Some("Tracking disabled for raw_mode or no_enter commands".to_string())
            } else {
                None
            },
            // No marker-bounded pane capture exists yet. Raw/no-enter output is a
            // diagnostic, while tracked commands begin queued with no output at all.
            output_truncated: true,
            reason: None,
            started_at: Instant::now(),
            completed_at: None,
            raw_mode,
            tracking_disabled,
        };

        {
            let mut commands = self.active_commands.write().await;
            commands.insert(command_id.clone(), execution.clone());
        }
        if let Some(ref secret) = secret {
            let mut secrets = self.secrets.write().await;
            secrets.insert(command_id.clone(), secret.clone());
        }

        self.emit(CommandEventKind::Created, &execution);
        self.cleanup_completed().await;

        if tracking_disabled {
            self.dispatch_keys(
                pane_id,
                command,
                delay_ms,
                no_enter,
                resolved_socket.as_deref(),
                &command_id,
            )
            .await?;
            return Ok(command_id);
        }

        let secret = secret.expect("tracked mode always has a secret");
        let key = Self::pane_key(pane_id, resolved_socket.as_deref());
        let launch = QueuedLaunch {
            command_id: command_id.clone(),
            pane_id: pane_id.to_string(),
            command: command.to_string(),
            delay_ms,
            socket: resolved_socket.clone(),
            secret,
        };

        let start_now = {
            let mut running = self.pane_running.write().await;
            match running.entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(_) => {
                    let mut queues = self.pane_queues.write().await;
                    queues.entry(key).or_default().push_back(launch.clone());
                    false
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(command_id.clone());
                    true
                }
            }
        };

        if start_now {
            self.start_tracked_launch(launch).await?;
        }

        Ok(command_id)
    }

    async fn start_tracked_launch(&self, launch: QueuedLaunch) -> Result<()> {
        run_tracked_launch(
            Arc::clone(&self.active_commands),
            Arc::clone(&self.secrets),
            Arc::clone(&self.pane_queues),
            Arc::clone(&self.pane_running),
            self.events.clone(),
            Arc::clone(&self.notify),
            self.tracking.clone(),
            self.shell_type,
            launch,
            false,
        )
        .await
    }

    async fn dispatch_keys(
        &self,
        pane_id: &str,
        wrapped_command: &str,
        delay_ms: Option<u64>,
        no_enter: bool,
        socket: Option<&str>,
        command_id: &str,
    ) -> Result<()> {
        if let Some(delay) = delay_ms {
            for ch in wrapped_command.chars() {
                if let Err(e) = tmux::send_keys(pane_id, &ch.to_string(), true, socket).await {
                    self.remove_command(command_id).await;
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if !no_enter {
                if let Err(e) = tmux::send_keys(pane_id, "Enter", false, socket).await {
                    self.remove_command(command_id).await;
                    return Err(e);
                }
            }
        } else {
            if let Err(e) = tmux::send_keys(pane_id, wrapped_command, false, socket).await {
                self.remove_command(command_id).await;
                return Err(e);
            }
            if !no_enter {
                if let Err(e) = tmux::send_keys(pane_id, "Enter", false, socket).await {
                    self.remove_command(command_id).await;
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    async fn remove_command(&self, command_id: &str) {
        let mut commands = self.active_commands.write().await;
        commands.remove(command_id);
        let mut secrets = self.secrets.write().await;
        secrets.remove(command_id);
    }

    /// Return the latest execution snapshot without using scrollback as an oracle.
    ///
    /// While `Running`, may refresh partial pane output for convenience.
    pub async fn check_status(
        &self,
        command_id: &str,
        socket_override: Option<&str>,
    ) -> Result<Option<CommandExecution>> {
        self.cleanup_completed().await;

        let mut execution = {
            let commands = self.active_commands.read().await;
            match commands.get(command_id) {
                Some(e) => e.clone(),
                None => return Ok(None),
            }
        };

        if execution.status.is_terminal() || execution.tracking_disabled || execution.raw_mode {
            return Ok(Some(execution));
        }

        if execution.status == CommandStatus::Running {
            let lines = partial_capture_lines(&self.tracking);
            let captured = capture_running_output(
                &execution.pane_id,
                &execution.id,
                lines,
                execution.socket.as_deref().or(socket_override),
            )
            .await;

            // The side-channel watcher may have committed terminal state while capture-pane
            // was in flight. Re-read under the write lock so that commit always wins and a
            // stale Running snapshot never escapes after terminal state is durable.
            let mut commands = self.active_commands.write().await;
            let stored = match commands.get_mut(command_id) {
                Some(stored) => stored,
                None => return Ok(None),
            };
            if !stored.status.is_terminal() {
                apply_captured_output(stored, captured);
            }
            execution = stored.clone();
        }

        Ok(Some(execution))
    }

    /// Block until the command is terminal or `wait_ms` elapses.
    ///
    /// Returns `(execution, wait_timed_out)`. Timeout does not change command status.
    pub async fn wait_for(
        &self,
        command_id: &str,
        wait_ms: u64,
    ) -> Result<Option<(CommandExecution, bool)>> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            if let Some(exec) = self.get_command(command_id).await {
                if exec.status.is_terminal() {
                    let exec = self.check_status(command_id, None).await?.unwrap_or(exec);
                    return Ok(Some((exec, false)));
                }
            } else {
                return Ok(None);
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let exec = self.check_status(command_id, None).await?;
                return Ok(exec.map(|e| (e, true)));
            }

            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(remaining.min(Duration::from_millis(50))) => {}
            }
        }
    }

    /// Snapshot a tracked command without side effects beyond memory read.
    pub async fn get_command(&self, id: &str) -> Option<CommandExecution> {
        let commands = self.active_commands.read().await;
        commands.get(id).cloned()
    }

    /// List ids currently held in the tracker map.
    pub async fn get_active_ids(&self) -> Vec<String> {
        let commands = self.active_commands.read().await;
        commands.keys().cloned().collect()
    }

    /// True if a command record exists (including terminal until eviction).
    pub async fn has_command(&self, id: &str) -> bool {
        let commands = self.active_commands.read().await;
        commands.contains_key(id)
    }

    /// Drop every tracked entry bound to a pane on one tmux socket.
    ///
    /// Queue/running maps are keyed by `socket|pane_id` because pane ids are
    /// only unique within a tmux server—do not purge other sockets' queues.
    pub async fn purge_pane(&self, pane_id: &str, socket: Option<&str>) -> usize {
        let mut commands = self.active_commands.write().await;
        let mut secrets = self.secrets.write().await;
        let mut removed = 0usize;
        let ids: Vec<String> = commands
            .iter()
            .filter(|(_, e)| e.pane_id == pane_id && e.socket.as_deref() == socket)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(exec) = commands.remove(&id) {
                secrets.remove(&id);
                removed += 1;
                let _ = self.events.send(CommandEvent {
                    command_id: exec.id.clone(),
                    resource_uri: command_resource_uri(&exec.id),
                    kind: CommandEventKind::Evicted,
                    status: exec.status,
                });
            }
        }
        {
            let key = Self::pane_key(pane_id, socket);
            let mut queues = self.pane_queues.write().await;
            queues.remove(&key);
            let mut running = self.pane_running.write().await;
            running.remove(&key);
        }
        self.notify.notify_waiters();
        removed
    }

    /// Remove completed commands outside the configured retention window and count.
    async fn cleanup_completed(&self) {
        let retention_minutes = self.tracking.completed_retention_minutes;
        let retention_window = Duration::from_secs(retention_minutes.saturating_mul(60));
        let now = Instant::now();

        let pending_abandon_window = Duration::from_secs(self.tracking.tracking_deadline_seconds)
            .saturating_add(retention_window)
            .max(Duration::from_secs(1));

        let mut evicted: Vec<CommandExecution> = Vec::new();
        {
            let mut commands = self.active_commands.write().await;
            let mut secrets = self.secrets.write().await;
            commands.retain(|id, exec| {
                let keep = if !exec.status.is_terminal() {
                    let age = now
                        .checked_duration_since(exec.started_at)
                        .unwrap_or(Duration::ZERO);
                    age < pending_abandon_window
                } else {
                    let completed_at = match exec.completed_at {
                        Some(instant) => instant,
                        None => return true,
                    };
                    let age = now
                        .checked_duration_since(completed_at)
                        .unwrap_or(Duration::ZERO);
                    age < retention_window
                };
                if !keep {
                    secrets.remove(id);
                    evicted.push(exec.clone());
                }
                keep
            });

            let max_entries = self.tracking.completed_max_entries as usize;
            if max_entries > 0 {
                let mut completed: Vec<(String, Instant)> = commands
                    .iter()
                    .filter_map(|(id, exec)| {
                        if !exec.status.is_terminal() {
                            return None;
                        }
                        exec.completed_at
                            .map(|completed_at| (id.clone(), completed_at))
                    })
                    .collect();

                if completed.len() > max_entries {
                    completed.sort_by_key(|(_, completed_at)| *completed_at);
                    let excess = completed.len().saturating_sub(max_entries);
                    for (id, _) in completed.into_iter().take(excess) {
                        if let Some(exec) = commands.remove(&id) {
                            secrets.remove(&id);
                            evicted.push(exec);
                        }
                    }
                }
            }
        }

        for exec in evicted {
            let _ = self.events.send(CommandEvent {
                command_id: exec.id.clone(),
                resource_uri: command_resource_uri(&exec.id),
                kind: CommandEventKind::Evicted,
                status: exec.status,
            });
        }
        self.notify.notify_waiters();
    }
}

/// Launch a tracked command and spawn its side-channel watcher (free fn so tasks stay `Send`).
#[allow(clippy::too_many_arguments)]
async fn run_tracked_launch(
    active_commands: Arc<RwLock<HashMap<String, CommandExecution>>>,
    secrets: Arc<RwLock<HashMap<String, String>>>,
    pane_queues: Arc<RwLock<HashMap<String, VecDeque<QueuedLaunch>>>>,
    pane_running: Arc<RwLock<HashMap<String, String>>>,
    events: broadcast::Sender<CommandEvent>,
    notify: Arc<Notify>,
    tracking: TrackingConfig,
    shell_type: ShellType,
    launch: QueuedLaunch,
    launch_was_queued: bool,
) -> Result<()> {
    let QueuedLaunch {
        command_id,
        pane_id,
        command,
        delay_ms,
        socket,
        secret,
    } = launch;

    let marker_shell = match tmux::pane_info(&pane_id, socket.as_deref()).await {
        Ok(info) => shell_type_for_pane_command(shell_type, &info.current_command),
        _ => shell_type,
    };
    let wrapped = wrap_tracked_command_side_channel(
        &command,
        &command_id,
        &secret,
        &marker_shell,
        socket.as_deref(),
    );

    let running_snapshot = {
        let mut commands = active_commands.write().await;
        if let Some(exec) = commands.get_mut(&command_id) {
            exec.status = CommandStatus::Running;
            exec.started_at = Instant::now();
            Some(exec.clone())
        } else {
            None
        }
    };
    if let Some(exec) = running_snapshot {
        let _ = events.send(CommandEvent {
            command_id: exec.id.clone(),
            resource_uri: command_resource_uri(&exec.id),
            kind: CommandEventKind::Updated,
            status: exec.status,
        });
        notify.notify_waiters();
    }

    if let Err(e) = dispatch_keys_free(&pane_id, &wrapped, delay_ms, socket.as_deref()).await {
        {
            let mut commands = active_commands.write().await;
            if launch_was_queued {
                if let Some(exec) = commands.get_mut(&command_id) {
                    if !exec.status.is_terminal() {
                        exec.status = CommandStatus::TrackingError;
                        exec.reason = Some(format!("failed to send command to pane: {e}"));
                        exec.completed_at = Some(Instant::now());
                        let _ = events.send(CommandEvent {
                            command_id: exec.id.clone(),
                            resource_uri: command_resource_uri(&exec.id),
                            kind: CommandEventKind::Terminal,
                            status: exec.status,
                        });
                    }
                }
            } else {
                commands.remove(&command_id);
            }
            secrets.write().await.remove(&command_id);
        }
        notify.notify_waiters();
        spawn_advance_queue(
            active_commands,
            secrets,
            pane_queues,
            pane_running,
            events,
            notify,
            tracking,
            shell_type,
            pane_id,
            socket,
        );
        return Err(e);
    }

    spawn_side_channel_watcher(
        active_commands,
        secrets,
        pane_queues,
        pane_running,
        events,
        notify,
        tracking,
        shell_type,
        command_id,
        pane_id,
        socket,
        secret,
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn spawn_side_channel_watcher(
    active_commands: Arc<RwLock<HashMap<String, CommandExecution>>>,
    secrets: Arc<RwLock<HashMap<String, String>>>,
    pane_queues: Arc<RwLock<HashMap<String, VecDeque<QueuedLaunch>>>>,
    pane_running: Arc<RwLock<HashMap<String, String>>>,
    events: broadcast::Sender<CommandEvent>,
    notify: Arc<Notify>,
    tracking: TrackingConfig,
    shell_type: ShellType,
    command_id: String,
    pane_id: String,
    socket: Option<String>,
    secret: String,
) {
    tokio::spawn(async move {
        let deadline = Duration::from_secs(tracking.tracking_deadline_seconds);
        let channel = tmux::wait_signal_name(&secret);
        let wait_result =
            tokio::time::timeout(deadline, tmux::wait_for_signal(&channel, socket.as_deref()))
                .await;

        let (status, exit_code, reason, final_capture) = match wait_result {
            Ok(Ok(())) => match tmux::read_exit_code_buffer(&secret, socket.as_deref()).await {
                Ok(exit_code) => (
                    if exit_code == 0 {
                        CommandStatus::Completed
                    } else {
                        CommandStatus::Failed
                    },
                    Some(exit_code),
                    None,
                    true,
                ),
                Err(e) => (
                    CommandStatus::TrackingError,
                    None,
                    Some(format!("side-channel exit buffer unreadable: {e}")),
                    true,
                ),
            },
            Ok(Err(e)) => (
                CommandStatus::TrackingError,
                None,
                Some(format!("wait-for failed: {e}")),
                false,
            ),
            Err(_) => (
                CommandStatus::TrackingError,
                None,
                Some("tracking deadline exceeded waiting for side channel".to_string()),
                false,
            ),
        };

        // Commit lifecycle state before any presentation-only pane capture. A stalled
        // local/SSH capture must not keep a side-channel-authoritative command Running.
        let terminal_snapshot = {
            let mut commands = active_commands.write().await;
            commands.get_mut(&command_id).and_then(|exec| {
                if exec.status.is_terminal() {
                    return None;
                }
                exec.status = status;
                exec.exit_code = exit_code;
                exec.output_truncated = true;
                exec.reason = reason;
                exec.completed_at = Some(Instant::now());
                Some(exec.clone())
            })
        };
        if let Some(exec) = &terminal_snapshot {
            let _ = events.send(CommandEvent {
                command_id: exec.id.clone(),
                resource_uri: command_resource_uri(&exec.id),
                kind: CommandEventKind::Terminal,
                status: exec.status,
            });
            notify.notify_waiters();
        }

        spawn_advance_queue(
            Arc::clone(&active_commands),
            Arc::clone(&secrets),
            pane_queues,
            pane_running,
            events.clone(),
            Arc::clone(&notify),
            tracking.clone(),
            shell_type,
            pane_id.clone(),
            socket.clone(),
        );

        secrets.write().await.remove(&command_id);
        let cleanup_socket = socket.clone();
        let cleanup_secret = secret.clone();
        tokio::spawn(async move {
            let _ = tmux::delete_exit_code_buffer(&cleanup_secret, cleanup_socket.as_deref()).await;
        });

        if terminal_snapshot.is_some() {
            let max_lines = tracking.capture_max_lines.max(1);
            let captured = if final_capture {
                capture_terminal_output(&pane_id, &command_id, max_lines, socket.as_deref()).await
            } else {
                let mut captured =
                    capture_running_output(&pane_id, &command_id, max_lines, socket.as_deref())
                        .await;
                // A tracking error without the completion signal means the command's output
                // boundary is unknown even when START remains visible.
                captured.truncated = true;
                captured
            };

            let updated_snapshot = {
                let mut commands = active_commands.write().await;
                commands.get_mut(&command_id).and_then(|exec| {
                    if !exec.status.is_terminal() {
                        return None;
                    }
                    let previous_output = exec.output.clone();
                    let previous_truncated = exec.output_truncated;
                    apply_captured_output(exec, captured);
                    (exec.output != previous_output || exec.output_truncated != previous_truncated)
                        .then(|| exec.clone())
                })
            };
            if let Some(exec) = updated_snapshot {
                let _ = events.send(CommandEvent {
                    command_id: exec.id.clone(),
                    resource_uri: command_resource_uri(&exec.id),
                    kind: CommandEventKind::Updated,
                    status: exec.status,
                });
                notify.notify_waiters();
            }
        }
    });
}

/// Pop the next queued launch for a pane and spawn `run_tracked_launch` (non-async to break cycles).
#[allow(clippy::too_many_arguments)]
fn spawn_advance_queue(
    active_commands: Arc<RwLock<HashMap<String, CommandExecution>>>,
    secrets: Arc<RwLock<HashMap<String, String>>>,
    pane_queues: Arc<RwLock<HashMap<String, VecDeque<QueuedLaunch>>>>,
    pane_running: Arc<RwLock<HashMap<String, String>>>,
    events: broadcast::Sender<CommandEvent>,
    notify: Arc<Notify>,
    tracking: TrackingConfig,
    shell_type: ShellType,
    pane_id: String,
    socket: Option<String>,
) {
    tokio::spawn(async move {
        let key = CommandTracker::pane_key(&pane_id, socket.as_deref());
        let next = {
            let mut running = pane_running.write().await;
            running.remove(&key);
            let mut queues = pane_queues.write().await;
            let next = queues.get_mut(&key).and_then(|q| q.pop_front());
            if let Some(ref n) = next {
                running.insert(key.clone(), n.command_id.clone());
            }
            if queues.get(&key).is_some_and(|q| q.is_empty()) {
                queues.remove(&key);
            }
            next
        };
        if let Some(launch) = next {
            let _ = run_tracked_launch(
                active_commands,
                secrets,
                pane_queues,
                pane_running,
                events,
                notify,
                tracking,
                shell_type,
                launch,
                true,
            )
            .await;
        }
    });
}

async fn dispatch_keys_free(
    pane_id: &str,
    wrapped_command: &str,
    delay_ms: Option<u64>,
    socket: Option<&str>,
) -> Result<()> {
    if let Some(delay) = delay_ms {
        for ch in wrapped_command.chars() {
            tmux::send_keys(pane_id, &ch.to_string(), true, socket).await?;
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        tmux::send_keys(pane_id, "Enter", false, socket).await?;
    } else {
        tmux::send_keys(pane_id, wrapped_command, false, socket).await?;
        tmux::send_keys(pane_id, "Enter", false, socket).await?;
    }
    Ok(())
}

fn partial_capture_lines(tracking: &TrackingConfig) -> u32 {
    tracking
        .capture_initial_lines
        .max(1)
        .min(tracking.capture_max_lines.max(1))
}

fn apply_captured_output(exec: &mut CommandExecution, captured: CapturedOutput) {
    if let Some(output) = captured.output {
        exec.output = Some(output);
    }
    exec.output_truncated = captured.truncated;
}

async fn capture_bracketed_output(
    pane_id: &str,
    command_id: &str,
    max_lines: u32,
    socket: Option<&str>,
) -> Result<BracketedOutput> {
    let captured =
        tmux::capture_pane(pane_id, Some(max_lines), false, None, None, true, socket).await?;
    Ok(extract_output_between_markers(&captured, command_id))
}

async fn capture_running_output(
    pane_id: &str,
    command_id: &str,
    max_lines: u32,
    socket: Option<&str>,
) -> CapturedOutput {
    match capture_bracketed_output(pane_id, command_id, max_lines, socket).await {
        Ok(BracketedOutput::Complete(output) | BracketedOutput::Open(output)) => CapturedOutput {
            output: Some(output),
            truncated: false,
        },
        Ok(BracketedOutput::MissingStart) | Err(_) => CapturedOutput::unavailable(),
    }
}

async fn capture_terminal_output(
    pane_id: &str,
    command_id: &str,
    max_lines: u32,
    socket: Option<&str>,
) -> CapturedOutput {
    let mut best_open_output = None;
    for attempt in 0..FINAL_OUTPUT_CAPTURE_ATTEMPTS {
        match capture_bracketed_output(pane_id, command_id, max_lines, socket).await {
            Ok(BracketedOutput::Complete(output)) => {
                return CapturedOutput {
                    output: Some(output),
                    truncated: false,
                };
            }
            Ok(BracketedOutput::Open(output)) => best_open_output = Some(output),
            Ok(BracketedOutput::MissingStart) => {
                return CapturedOutput {
                    output: best_open_output,
                    truncated: true,
                };
            }
            Err(_) => {}
        }

        if attempt + 1 < FINAL_OUTPUT_CAPTURE_ATTEMPTS {
            tokio::time::sleep(FINAL_OUTPUT_CAPTURE_RETRY_DELAY).await;
        }
    }

    CapturedOutput {
        output: best_open_output,
        truncated: true,
    }
}

/// Extract bounded output markers for presentation only, never command completion.
fn extract_output_between_markers(captured: &str, command_id: &str) -> BracketedOutput {
    let start_marker = get_start_marker(command_id);
    let after_start = match captured.rfind(&start_marker) {
        Some(start_idx) => &captured[start_idx + start_marker.len()..],
        None => return BracketedOutput::MissingStart,
    };
    let end_prefix = end_marker_prefix(command_id);
    let Some(end_regex) = Regex::new(&format!(r"{}(\d+)", regex::escape(&end_prefix))).ok() else {
        return BracketedOutput::MissingStart;
    };
    if let Some(m) = end_regex.find_iter(after_start).last() {
        BracketedOutput::Complete(after_start[..m.start()].trim().to_string())
    } else {
        BracketedOutput::Open(after_start.trim().to_string())
    }
}

/// START marker text echoed into pane scrollback for human/debug bracketing.
///
/// Not completion authority—exit status comes only from the private side channel.
pub fn get_start_marker(command_id: &str) -> String {
    format!("{START_MARKER_PREFIX}{command_id}")
}

fn end_marker_prefix(command_id: &str) -> String {
    format!("{END_MARKER_PREFIX}{command_id}_")
}

/// DONE marker body (prefix + shell exit-status expansion) for debug echo only.
#[allow(dead_code)]
pub fn get_end_marker(shell: &ShellType, command_id: &str) -> String {
    let prefix = end_marker_prefix(command_id);
    match shell {
        ShellType::Fish => format!("{prefix}$status"),
        ShellType::Bash | ShellType::Zsh | ShellType::Unknown => format!("{prefix}$?"),
    }
}

/// Wrap a tracked command with side-channel completion + optional markers.
fn wrap_tracked_command_side_channel(
    command: &str,
    command_id: &str,
    secret: &str,
    shell: &ShellType,
    socket: Option<&str>,
) -> String {
    let start_marker = get_start_marker(command_id);
    let tmux_bin = tmux::shell_tmux_prefix(socket);
    let buf = tmux::exit_code_buffer_name(secret);
    let channel = tmux::wait_signal_name(secret);
    let buf_q = tmux::shell_single_quote(&buf);
    let chan_q = tmux::shell_single_quote(&channel);

    match shell {
        ShellType::Fish => {
            format!(
                "echo \"{start_marker}\"; {command} ; set __tmux_mcp_ec $status; {tmux_bin} set-buffer -b {buf_q} -- $__tmux_mcp_ec; {tmux_bin} wait-for -S {chan_q}; echo \"{END_MARKER_PREFIX}{command_id}_$__tmux_mcp_ec\""
            )
        }
        ShellType::Bash | ShellType::Zsh | ShellType::Unknown => {
            format!(
                "echo \"{start_marker}\"; {command} ; __tmux_mcp_ec=$?; {tmux_bin} set-buffer -b {buf_q} -- \"$__tmux_mcp_ec\"; {tmux_bin} wait-for -S {chan_q}; echo \"{END_MARKER_PREFIX}{command_id}_$__tmux_mcp_ec\""
            )
        }
    }
}

/// Prefer the pane's live shell binary over the process-wide default when wrapping markers.
///
/// Login shells (`-bash`) and absolute paths are normalized; unknown binaries keep
/// `configured_shell` so fish/zsh exit-status expansions stay correct when detectable.
fn shell_type_for_pane_command(configured_shell: ShellType, current_command: &str) -> ShellType {
    let command = current_command
        .rsplit('/')
        .next()
        .unwrap_or(current_command)
        .trim_start_matches('-');
    match command {
        "fish" => ShellType::Fish,
        "zsh" => ShellType::Zsh,
        "bash" | "sh" | "dash" | "ksh" => ShellType::Bash,
        _ => configured_shell,
    }
}

/// Legacy-style wrap used only in unit tests for marker string shape.
#[cfg(test)]
fn wrap_tracked_command(command: &str, start_marker: &str, end_marker: &str) -> String {
    format!("echo \"{start_marker}\"; {command} ; echo \"{end_marker}\"")
}

/// True when an unquoted `#` would start a shell comment and truncate the tracker wrapper.
fn has_unquoted_shell_comment_marker(command: &str) -> bool {
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    let mut at_word_start = true;

    for ch in command.chars() {
        if escaped {
            escaped = false;
            at_word_start = false;
            continue;
        }

        match ch {
            '\\' if !in_single_quote => escaped = true,
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
                at_word_start = false;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
                at_word_start = false;
            }
            '#' if !in_single_quote && !in_double_quote && at_word_start => return true,
            ch if !in_single_quote
                && !in_double_quote
                && (ch.is_whitespace()
                    || matches!(ch, ';' | '&' | '|' | '(' | ')' | '<' | '>')) =>
            {
                at_word_start = true;
            }
            _ => {
                at_word_start = false;
            }
        }
    }

    false
}

/// True when an unquoted `&` would background work and skip the side-channel epilogue.
///
/// Treats `&&` and `&>` as non-background operators so normal control flow still tracks.
fn has_unquoted_shell_background_operator(command: &str) -> bool {
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    let mut iter = command.chars().peekable();

    while let Some(ch) = iter.next() {
        if escaped {
            escaped = false;
            continue;
        }

        match ch {
            '\\' if !in_single_quote => {
                escaped = true;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            '&' if !in_single_quote && !in_double_quote => {
                let next = iter.peek().copied();
                if matches!(next, Some('&')) {
                    iter.next();
                    continue;
                }
                if matches!(next, Some('>')) {
                    continue;
                }

                return true;
            }
            _ => {}
        }
    }

    false
}

/// Parse markers for output bracketing only (not completion).
#[cfg(test)]
fn parse_command_output(captured: &str, command_id: &str) -> Option<(String, i32)> {
    let start_marker = get_start_marker(command_id);
    let after_start = match captured.rfind(&start_marker) {
        Some(start_idx) => &captured[start_idx + start_marker.len()..],
        None => captured,
    };
    let end_prefix = end_marker_prefix(command_id);
    let end_regex = Regex::new(&format!(r"{}(\d+)", regex::escape(&end_prefix))).ok()?;
    let last_match = end_regex.captures_iter(after_start).last()?;
    let exit_code: i32 = last_match.get(1)?.as_str().parse().ok()?;
    let end_match = last_match.get(0)?;
    let output = after_start[..end_match.start()].trim().to_string();
    Some((output, exit_code))
}

#[cfg(test)]
fn extract_exit_code(line: &str, command_id: &str) -> Option<i32> {
    let end_prefix = end_marker_prefix(command_id);
    if line.contains(&end_prefix) {
        let end_regex = Regex::new(&format!(r"{}(\d+)", regex::escape(&end_prefix))).ok()?;
        let caps = end_regex.captures(line)?;
        caps.get(1)?.as_str().parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::Error;
    use crate::test_support::TmuxStub;
    use crate::types::CommandSnapshot;
    use rstest::rstest;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::NamedTempFile;

    async fn wait_until_terminal(tracker: &CommandTracker, id: &str) -> CommandExecution {
        for _ in 0..100 {
            if let Some(cmd) = tracker.get_command(id).await {
                if cmd.status.is_terminal() {
                    return cmd;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tracker.get_command(id).await.expect("command should exist")
    }

    async fn wait_until_output_refresh(tracker: &CommandTracker, id: &str) -> CommandExecution {
        for _ in 0..100 {
            if let Some(cmd) = tracker.get_command(id).await {
                if cmd.status.is_terminal() && cmd.output.is_some() {
                    return cmd;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tracker.get_command(id).await.expect("command should exist")
    }

    fn assert_snapshot_output_incomplete(exec: &CommandExecution) {
        assert!(exec.output_truncated);
        let snapshot = CommandSnapshot::from_execution(exec, None);
        assert!(snapshot.output_truncated);
        let wire = serde_json::to_value(snapshot).expect("serialize command snapshot");
        assert_eq!(wire["outputTruncated"], true);
    }

    fn running_execution(id: &str) -> CommandExecution {
        CommandExecution {
            id: id.to_string(),
            pane_id: "%1".to_string(),
            socket: None,
            command: "printf partial".to_string(),
            status: CommandStatus::Running,
            exit_code: None,
            output: None,
            output_truncated: false,
            reason: None,
            started_at: Instant::now(),
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        }
    }

    #[rstest]
    #[case(ShellType::Bash, "TMUX_MCP_DONE_cmd-1_$?")]
    #[case(ShellType::Zsh, "TMUX_MCP_DONE_cmd-1_$?")]
    #[case(ShellType::Fish, "TMUX_MCP_DONE_cmd-1_$status")]
    #[case(ShellType::Unknown, "TMUX_MCP_DONE_cmd-1_$?")]
    fn test_get_end_marker(#[case] shell: ShellType, #[case] expected: &str) {
        assert_eq!(get_end_marker(&shell, "cmd-1"), expected);
    }

    #[rstest]
    #[case("TMUX_MCP_DONE_cmd-1_0", Some(0))]
    #[case("TMUX_MCP_DONE_cmd-1_1", Some(1))]
    #[case("no marker here", None)]
    fn test_extract_exit_code(#[case] input: &str, #[case] expected: Option<i32>) {
        assert_eq!(extract_exit_code(input, "cmd-1"), expected);
    }

    #[test]
    fn test_parse_command_output_brackets_only() {
        let input = "TMUX_MCP_START_cmd-1\nhello\nTMUX_MCP_DONE_cmd-1_0\n";
        assert_eq!(
            parse_command_output(input, "cmd-1"),
            Some(("hello".to_string(), 0))
        );
    }

    #[rstest]
    #[case(
        "TMUX_MCP_START_cmd-1\nhello\nTMUX_MCP_DONE_cmd-1_0\n",
        BracketedOutput::Complete("hello".to_string())
    )]
    #[case(
        "TMUX_MCP_START_cmd-1\nTMUX_MCP_DONE_cmd-1_0\n",
        BracketedOutput::Complete(String::new())
    )]
    #[case(
        "TMUX_MCP_START_cmd-1\npartial\n",
        BracketedOutput::Open("partial".to_string())
    )]
    #[case("tail without start", BracketedOutput::MissingStart)]
    fn extract_output_reports_capture_boundaries(
        #[case] captured: &str,
        #[case] expected: BracketedOutput,
    ) {
        assert_eq!(extract_output_between_markers(captured, "cmd-1"), expected);
    }

    #[test]
    fn extract_output_uses_last_done_marker_for_the_boundary() {
        let captured = concat!(
            "TMUX_MCP_START_cmd-1\n",
            "first\n",
            "TMUX_MCP_DONE_cmd-1_0\n",
            "last\n",
            "TMUX_MCP_DONE_cmd-1_0\n",
        );
        assert_eq!(
            extract_output_between_markers(captured, "cmd-1"),
            BracketedOutput::Complete("first\nTMUX_MCP_DONE_cmd-1_0\nlast".to_string())
        );
    }

    #[test]
    fn partial_capture_never_exceeds_final_capture_budget() {
        let mut tracking = TrackingConfig::default();
        tracking.capture_initial_lines = 10_000;
        tracking.capture_max_lines = 25;
        assert_eq!(partial_capture_lines(&tracking), 25);

        tracking.capture_initial_lines = 0;
        tracking.capture_max_lines = 0;
        assert_eq!(partial_capture_lines(&tracking), 1);
    }

    #[tokio::test]
    async fn partial_capture_is_bounded_and_preserves_useful_output_after_marker_loss() {
        let mut stub = TmuxStub::new();
        let capture_log = NamedTempFile::new().expect("capture log");
        stub.set_var("TMUX_STUB_CAPTURE_LOG", capture_log.path());
        stub.set_var(
            "TMUX_STUB_CAPTURE_OUTPUT",
            "TMUX_MCP_START_cmd-partial\nfirst partial\n",
        );

        let mut tracking = TrackingConfig::default();
        tracking.capture_initial_lines = 100;
        tracking.capture_max_lines = 2;
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);
        tracker
            .active_commands
            .write()
            .await
            .insert("cmd-partial".to_string(), running_execution("cmd-partial"));

        let first = tracker
            .check_status("cmd-partial", None)
            .await
            .expect("partial status")
            .expect("partial command");
        assert_eq!(first.output.as_deref(), Some("first partial"));
        assert!(!first.output_truncated);

        stub.set_var(
            "TMUX_STUB_CAPTURE_OUTPUT",
            "tail after START left scrollback",
        );
        let overflowed = tracker
            .check_status("cmd-partial", None)
            .await
            .expect("overflow status")
            .expect("overflow command");
        assert_eq!(overflowed.output.as_deref(), Some("first partial"));
        assert!(overflowed.output_truncated);

        let logged = std::fs::read_to_string(capture_log.path()).expect("read capture log");
        assert_eq!(logged.lines().count(), 2);
        assert!(
            logged.lines().all(|line| line.contains("-J -S -2 -E -")),
            "partial capture exceeded final line budget: {logged}"
        );
    }

    #[tokio::test]
    async fn terminal_capture_retries_boundedly_until_done_is_visible() {
        let mut stub = TmuxStub::new();
        let count_file = NamedTempFile::new().expect("capture count");
        stub.set_var("TMUX_STUB_CAPTURE_COUNT_FILE", count_file.path());
        stub.set_var("TMUX_STUB_CAPTURE_AFTER", "2");
        stub.set_var(
            "TMUX_STUB_CAPTURE_BEFORE",
            "TMUX_MCP_START_cmd-final\nfinal output\n",
        );
        stub.set_var(
            "TMUX_STUB_CAPTURE_AFTER_OUTPUT",
            "TMUX_MCP_START_cmd-final\nfinal output\nTMUX_MCP_DONE_cmd-final_0\n",
        );

        let captured = capture_terminal_output("%1", "cmd-final", 8, None).await;
        assert_eq!(captured.output.as_deref(), Some("final output"));
        assert!(!captured.truncated);
        assert_eq!(
            std::fs::read_to_string(count_file.path())
                .expect("read capture count")
                .trim(),
            "2"
        );
    }

    #[tokio::test]
    async fn terminal_capture_marks_missing_boundaries_truncated() {
        let mut stub = TmuxStub::new();
        let capture_log = NamedTempFile::new().expect("capture log");
        stub.set_var("TMUX_STUB_CAPTURE_LOG", capture_log.path());
        stub.set_var(
            "TMUX_STUB_CAPTURE_OUTPUT",
            "TMUX_MCP_START_cmd-final\nbounded tail\n",
        );

        let missing_done = capture_terminal_output("%1", "cmd-final", 4, None).await;
        assert_eq!(missing_done.output.as_deref(), Some("bounded tail"));
        assert!(missing_done.truncated);
        let logged = std::fs::read_to_string(capture_log.path()).expect("read capture log");
        assert_eq!(logged.lines().count(), FINAL_OUTPUT_CAPTURE_ATTEMPTS);

        stub.set_var(
            "TMUX_STUB_CAPTURE_OUTPUT",
            "left-truncated tail\nTMUX_MCP_DONE_cmd-final_0\n",
        );
        let missing_start = capture_terminal_output("%1", "cmd-final", 4, None).await;
        assert_eq!(missing_start.output, None);
        assert!(missing_start.truncated);
    }

    #[tokio::test]
    async fn check_status_returns_terminal_commit_that_wins_capture_race() {
        let mut stub = TmuxStub::new();
        let capture_log = NamedTempFile::new().expect("capture log");
        stub.set_var("TMUX_STUB_CAPTURE_LOG", capture_log.path());
        stub.set_var("TMUX_STUB_CAPTURE_SLEEP_SECS", "1");
        stub.set_var(
            "TMUX_STUB_CAPTURE_OUTPUT",
            "TMUX_MCP_START_cmd-race\nstale partial\n",
        );

        let tracker = Arc::new(CommandTracker::new(ShellType::Bash));
        tracker
            .active_commands
            .write()
            .await
            .insert("cmd-race".to_string(), running_execution("cmd-race"));

        let status_task = {
            let tracker = Arc::clone(&tracker);
            tokio::spawn(async move { tracker.check_status("cmd-race", None).await })
        };

        let mut capture_started = false;
        // Other command tests may still have short-lived watcher subprocesses occupying
        // the shared tmux semaphore; wait long enough for those bounded tasks to drain.
        for _ in 0..500 {
            if std::fs::metadata(capture_log.path()).is_ok_and(|metadata| metadata.len() > 0) {
                capture_started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(capture_started, "capture-pane did not start");

        {
            let mut commands = tracker.active_commands.write().await;
            let committed = commands.get_mut("cmd-race").expect("race command");
            committed.status = CommandStatus::Completed;
            committed.exit_code = Some(0);
            committed.output = Some("authoritative final".to_string());
            committed.output_truncated = false;
            committed.completed_at = Some(Instant::now());
        }

        let observed = status_task
            .await
            .expect("join status task")
            .expect("status result")
            .expect("race command");
        assert_eq!(observed.status, CommandStatus::Completed);
        assert_eq!(observed.output.as_deref(), Some("authoritative final"));
        assert!(!observed.output_truncated);
    }

    #[test]
    fn test_wrap_includes_side_channel() {
        let wrapped = wrap_tracked_command_side_channel(
            "true",
            "cmd-1",
            "deadbeef",
            &ShellType::Bash,
            Some("/tmp/t.sock"),
        );
        assert!(wrapped.contains("wait-for -S"));
        assert!(wrapped.contains("tmux-mcp-ec-deadbeef"));
        assert!(wrapped.contains("TMUX_MCP_START_cmd-1"));
        assert!(wrapped.contains("-S '/tmp/t.sock'") || wrapped.contains("-S /tmp/t.sock"));
    }

    #[rstest]
    #[case(ShellType::Fish, "bash", ShellType::Bash)]
    #[case(ShellType::Fish, "zsh", ShellType::Zsh)]
    #[case(ShellType::Bash, "fish", ShellType::Fish)]
    #[case(ShellType::Bash, "/bin/fish", ShellType::Fish)]
    #[case(ShellType::Fish, "vim", ShellType::Fish)]
    fn test_shell_type_for_pane_command(
        #[case] configured: ShellType,
        #[case] current_command: &str,
        #[case] expected: ShellType,
    ) {
        assert_eq!(
            shell_type_for_pane_command(configured, current_command),
            expected
        );
    }

    #[test]
    fn test_resource_uri_helper() {
        assert_eq!(command_resource_uri("abc"), "tmux://command/abc/result");
    }

    #[tokio::test]
    async fn execute_command_side_channel_completes() {
        let _stub = TmuxStub::new();
        let tracker = CommandTracker::new(ShellType::Bash);
        let id = tracker
            .execute_command("%1", "echo hi", false, false, None, None)
            .await
            .expect("execute");
        let terminal = wait_until_terminal(&tracker, &id).await;
        assert_eq!(terminal.status, CommandStatus::Completed);
        assert_eq!(terminal.exit_code, Some(0));
    }

    #[tokio::test]
    async fn execute_command_side_channel_retries_output_until_done_marker_is_visible() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_WAIT_FOR_SLEEP_SECS", "1");
        let count_file = NamedTempFile::new().expect("capture count");
        let tracker = CommandTracker::new(ShellType::Bash);
        let id = tracker
            .execute_command("%1", "printf final", false, false, None, None)
            .await
            .expect("execute");

        stub.set_var("TMUX_STUB_CAPTURE_COUNT_FILE", count_file.path());
        stub.set_var("TMUX_STUB_CAPTURE_AFTER", "2");
        stub.set_var(
            "TMUX_STUB_CAPTURE_BEFORE",
            format!("TMUX_MCP_START_{id}\nfinal output\n"),
        );
        stub.set_var(
            "TMUX_STUB_CAPTURE_AFTER_OUTPUT",
            format!("TMUX_MCP_START_{id}\nfinal output\nTMUX_MCP_DONE_{id}_0\n"),
        );

        let terminal = wait_until_terminal(&tracker, &id).await;
        assert_eq!(terminal.status, CommandStatus::Completed);
        assert_eq!(terminal.exit_code, Some(0));
        let cmd = wait_until_output_refresh(&tracker, &id).await;
        assert_eq!(cmd.status, CommandStatus::Completed);
        assert_eq!(cmd.exit_code, Some(0));
        assert_eq!(cmd.output.as_deref(), Some("final output"));
        assert!(!cmd.output_truncated);
        assert_eq!(
            std::fs::read_to_string(count_file.path())
                .expect("read capture count")
                .trim(),
            "2"
        );
    }

    #[tokio::test]
    async fn execute_command_side_channel_keeps_status_but_marks_unclosed_output_truncated() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_WAIT_FOR_SLEEP_SECS", "1");
        let capture_log = NamedTempFile::new().expect("capture log");
        let tracker = CommandTracker::new(ShellType::Bash);
        let id = tracker
            .execute_command("%1", "printf tail", false, false, None, None)
            .await
            .expect("execute");

        stub.set_var("TMUX_STUB_CAPTURE_LOG", capture_log.path());
        stub.set_var(
            "TMUX_STUB_CAPTURE_OUTPUT",
            format!("TMUX_MCP_START_{id}\nbounded tail\n"),
        );

        let terminal = wait_until_terminal(&tracker, &id).await;
        assert_eq!(terminal.status, CommandStatus::Completed);
        assert_eq!(terminal.exit_code, Some(0));
        let cmd = wait_until_output_refresh(&tracker, &id).await;
        assert_eq!(cmd.output.as_deref(), Some("bounded tail"));
        assert!(cmd.output_truncated);
        let logged = std::fs::read_to_string(capture_log.path()).expect("read capture log");
        assert_eq!(logged.lines().count(), FINAL_OUTPUT_CAPTURE_ATTEMPTS);
    }

    #[tokio::test]
    async fn execute_command_spoof_done_does_not_complete() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_WAIT_FOR_SLEEP_SECS", "2");
        let tracker = CommandTracker::new(ShellType::Bash);
        let id = tracker
            .execute_command("%1", "sleep 30", false, false, None, None)
            .await
            .expect("execute");

        stub.set_var(
            "TMUX_STUB_CAPTURE_OUTPUT",
            format!("TMUX_MCP_START_{id}\nTMUX_MCP_DONE_{id}_0\n"),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        let cmd = tracker
            .check_status(&id, None)
            .await
            .expect("status")
            .expect("found");
        assert!(
            !cmd.status.is_terminal(),
            "scrollback DONE must not complete; got {:?}",
            cmd.status
        );
    }

    #[tokio::test]
    async fn execute_command_queues_second_on_same_pane() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_WAIT_FOR_SLEEP_SECS", "1");
        let tracker = CommandTracker::new(ShellType::Bash);

        let id1 = tracker
            .execute_command("%1", "echo one", false, false, None, None)
            .await
            .expect("first");
        let id2 = tracker
            .execute_command("%1", "echo two", false, false, None, None)
            .await
            .expect("second");

        let c2 = tracker.get_command(&id2).await.expect("c2");
        assert_eq!(
            c2.status,
            CommandStatus::Queued,
            "second tracked command should queue"
        );
        assert_eq!(c2.output, None);
        assert_snapshot_output_incomplete(&c2);

        let c1 = wait_until_terminal(&tracker, &id1).await;
        assert!(c1.status.is_terminal());
        let c2 = wait_until_terminal(&tracker, &id2).await;
        assert!(c2.status.is_terminal());
    }

    #[tokio::test]
    async fn terminal_state_and_queue_advance_while_presentation_capture_is_blocked() {
        let mut stub = TmuxStub::new();
        let capture_log = NamedTempFile::new().expect("capture log");
        stub.set_var("TMUX_STUB_WAIT_FOR_SLEEP_SECS", "0.1");
        stub.set_var("TMUX_STUB_CAPTURE_SLEEP_SECS", "2");
        stub.set_var("TMUX_STUB_CAPTURE_LOG", capture_log.path());

        let mut tracking = TrackingConfig::default();
        tracking.tracking_deadline_seconds = 1;
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);
        let id1 = tracker
            .execute_command("%1", "echo one", false, false, None, None)
            .await
            .expect("first");
        let id2 = tracker
            .execute_command("%1", "echo two", false, false, None, None)
            .await
            .expect("second");
        assert_eq!(
            tracker.get_command(&id2).await.expect("queued").status,
            CommandStatus::Queued
        );

        let mut capture_started = false;
        for _ in 0..100 {
            if std::fs::metadata(capture_log.path()).is_ok_and(|metadata| metadata.len() > 0) {
                capture_started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(capture_started, "presentation capture did not start");

        let first = tokio::time::timeout(
            Duration::from_millis(1_200),
            wait_until_terminal(&tracker, &id1),
        )
        .await
        .expect("first status blocked on capture");
        assert_eq!(first.status, CommandStatus::Completed);
        assert_snapshot_output_incomplete(&first);

        let second = tokio::time::timeout(
            Duration::from_millis(1_200),
            wait_until_terminal(&tracker, &id2),
        )
        .await
        .expect("pane queue blocked on first capture");
        assert_eq!(second.status, CommandStatus::Completed);
        assert_snapshot_output_incomplete(&second);

        // Let both deliberately slow stub captures finish before restoring the environment.
        tokio::time::sleep(Duration::from_millis(2_300)).await;
    }

    #[tokio::test]
    async fn queued_send_failure_remains_pollable_as_tracking_error() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_WAIT_FOR_SLEEP_SECS", "1");
        let tracker = CommandTracker::new(ShellType::Bash);

        let id1 = tracker
            .execute_command("%1", "echo one", false, false, None, None)
            .await
            .expect("first");
        let id2 = tracker
            .execute_command("%1", "echo two", false, false, None, None)
            .await
            .expect("second");
        assert_eq!(
            tracker
                .get_command(&id2)
                .await
                .expect("queued command")
                .status,
            CommandStatus::Queued
        );

        stub.set_var("TMUX_STUB_ERROR_CMD", "send-keys");
        let _ = wait_until_terminal(&tracker, &id1).await;
        let failed = wait_until_terminal(&tracker, &id2).await;

        assert_eq!(failed.status, CommandStatus::TrackingError);
        assert!(failed.completed_at.is_some());
        assert_eq!(failed.output, None);
        assert_snapshot_output_incomplete(&failed);
        assert!(
            failed
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("failed to send command to pane")),
            "unexpected failure reason: {:?}",
            failed.reason
        );
    }

    #[tokio::test]
    async fn wait_for_returns_on_completion() {
        let _stub = TmuxStub::new();
        let tracker = CommandTracker::new(ShellType::Bash);
        let id = tracker
            .execute_command("%1", "true", false, false, None, None)
            .await
            .expect("execute");
        let (cmd, timed_out) = tracker
            .wait_for(&id, 5_000)
            .await
            .expect("wait")
            .expect("found");
        assert!(!timed_out);
        assert_eq!(cmd.status, CommandStatus::Completed);
    }

    #[tokio::test]
    async fn wait_for_timeout_leaves_running() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_WAIT_FOR_SLEEP_SECS", "3");
        let tracker = CommandTracker::new(ShellType::Bash);
        let id = tracker
            .execute_command("%1", "sleep 99", false, false, None, None)
            .await
            .expect("execute");
        let (cmd, timed_out) = tracker
            .wait_for(&id, 100)
            .await
            .expect("wait")
            .expect("found");
        assert!(timed_out);
        assert!(
            !cmd.status.is_terminal(),
            "timeout must not terminalize; got {:?}",
            cmd.status
        );
    }

    #[tokio::test]
    async fn execute_command_rejects_newline() {
        let _stub = TmuxStub::new();
        let tracker = CommandTracker::new(ShellType::Bash);
        let err = tracker
            .execute_command("%1", "echo a\necho b", false, false, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[tokio::test]
    async fn execute_command_rejects_unquoted_hash() {
        let _stub = TmuxStub::new();
        let tracker = CommandTracker::new(ShellType::Bash);
        let err = tracker
            .execute_command("%1", "grep x # note", false, false, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[tokio::test]
    async fn execute_command_rejects_background() {
        let _stub = TmuxStub::new();
        let tracker = CommandTracker::new(ShellType::Bash);
        let err = tracker
            .execute_command("%1", "sleep 1 &", false, false, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[tokio::test]
    async fn execute_command_rejects_intra_word_background() {
        let _stub = TmuxStub::new();
        let tracker = CommandTracker::new(ShellType::Bash);
        let err = tracker
            .execute_command("%1", "sleep 60&true", false, false, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[tokio::test]
    async fn execute_command_uses_pane_shell_for_markers() {
        let mut stub = TmuxStub::new();
        let log = NamedTempFile::new().expect("log");
        stub.set_var("TMUX_STUB_SEND_KEYS_LOG", log.path());

        let tracker = CommandTracker::new(ShellType::Fish);
        tracker
            .execute_command("%1", "true", false, false, None, None)
            .await
            .expect("execute");

        let logged = std::fs::read_to_string(log.path()).expect("read log");
        assert!(
            logged.contains("__tmux_mcp_ec=$?"),
            "bash pane should use POSIX exit status syntax, got: {logged}"
        );
        assert!(
            !logged.contains("set __tmux_mcp_ec $status"),
            "bash pane must not receive fish syntax, got: {logged}"
        );
    }

    #[tokio::test]
    async fn execute_command_send_failure_rolls_back() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_ERROR_CMD", "send-keys");
        let tracker = CommandTracker::new(ShellType::Bash);
        let err = tracker
            .execute_command("%1", "echo hi", false, false, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Tmux { .. }));
        assert!(tracker.get_active_ids().await.is_empty());
    }

    #[tokio::test]
    async fn purge_pane_is_scoped_to_socket() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_WAIT_FOR_SLEEP_SECS", "2");
        let tracker = CommandTracker::new(ShellType::Bash);
        let socket_a = "/tmp/tmux-mcp-a.sock";
        let socket_b = "/tmp/tmux-mcp-b.sock";
        let id_a = tracker
            .execute_command("%1", "echo a", false, false, None, Some(socket_a.into()))
            .await
            .expect("execute on socket a");
        let queued_a = tracker
            .execute_command(
                "%1",
                "echo queued-a",
                false,
                false,
                None,
                Some(socket_a.into()),
            )
            .await
            .expect("queue on socket a");
        let id_b = tracker
            .execute_command("%1", "echo b", false, false, None, Some(socket_b.into()))
            .await
            .expect("execute on socket b");
        let queued_b = tracker
            .execute_command(
                "%1",
                "echo queued-b",
                false,
                false,
                None,
                Some(socket_b.into()),
            )
            .await
            .expect("queue on socket b");

        assert_eq!(tracker.purge_pane("%1", Some(socket_a)).await, 2);
        assert!(tracker.get_command(&id_a).await.is_none());
        assert!(tracker.get_command(&queued_a).await.is_none());
        assert!(tracker.get_command(&id_b).await.is_some());
        assert!(tracker.get_command(&queued_b).await.is_some());
        assert_eq!(tracker.purge_pane("%1", Some(socket_b)).await, 2);
    }

    #[tokio::test]
    async fn raw_mode_disables_tracking() {
        let _stub = TmuxStub::new();
        let tracker = CommandTracker::new(ShellType::Bash);
        let id = tracker
            .execute_command("%1", "echo hi", true, false, None, None)
            .await
            .expect("execute");
        let cmd = tracker.get_command(&id).await.expect("found");
        assert!(cmd.tracking_disabled);
        assert_eq!(cmd.status, CommandStatus::Running);
        assert!(cmd.output.as_deref().is_some_and(|output| {
            output.contains("Tracking disabled for raw_mode or no_enter commands")
        }));
        assert_snapshot_output_incomplete(&cmd);
    }

    #[rstest]
    #[case("grep pattern # notes")]
    #[case("# all comment")]
    fn test_has_unquoted_shell_comment_marker_rejects(#[case] command: &str) {
        assert!(has_unquoted_shell_comment_marker(command));
    }

    #[rstest]
    #[case("echo '# literal'")]
    #[case(r#"echo "x#y""#)]
    fn test_has_unquoted_shell_comment_marker_allows(#[case] command: &str) {
        assert!(!has_unquoted_shell_comment_marker(command));
    }

    #[test]
    fn test_wrap_tracked_command_space_before_done() {
        let wrapped = wrap_tracked_command("grep foo\\", "START", "DONE");
        assert!(wrapped.contains("grep foo\\ ; echo"));
    }
}
