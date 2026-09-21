//! Side-channel command execution tracking for agent-driven shell work in panes.
//!
//! `CommandTracker` runs at most one tracked command per pane, wraps it with optional
//! START/DONE debug markers plus a private tmux exit-code side channel
//! (`set-buffer` + `wait-for`), and commits terminal status only from that
//! channel—not from forgeable pane scrollback.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Deserialize;
use tokio::sync::{broadcast, Notify, RwLock};
use uuid::Uuid;

use crate::errors::{Error, Result};
use crate::tmux;
#[cfg(test)]
use crate::types::command_resource_uri;
use crate::types::{CommandExecution, CommandStatus, ShellType};

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
    /// Command id that was created, updated, completed, or evicted.
    #[allow(dead_code)]
    pub command_id: String,
    /// Canonical `tmux://command/{id}/result` URI for subscription matching.
    pub resource_uri: String,
    pub kind: CommandEventKind,
    /// Status at the moment of the durable commit.
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
    /// Terminal status and its final bounded capture attempt are ready to read.
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
    #[allow(dead_code)] // Used by the public library's explicit check_status refresh API.
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
    /// Interval for checking whether a long-running side-channel waiter was purged.
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

/// How often a side-channel watcher checks whether its command was purged.
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
struct TrackedLaunch {
    command_id: String,
    pane_id: String,
    command: String,
    delay_ms: Option<u64>,
    socket: Option<String>,
    secret: String,
}

/// In-process registry of running and recently completed pane commands.
///
/// One tracker instance is shared by the MCP server. A second tracked launch for
/// the same `target|socket|pane_id` is rejected while the first command is unresolved.
/// Completion is side-channel authoritative; pane scrollback is presentation only.
pub struct CommandTracker {
    /// All live records keyed by command id (including terminal until eviction).
    active_commands: Arc<RwLock<HashMap<String, CommandExecution>>>,
    /// Private side-channel secrets (never exposed on CommandExecution).
    secrets: Arc<RwLock<HashMap<String, String>>>,
    /// `target|socket|pane_id` → current tracked command id. Tracking errors retain this
    /// entry when execution is uncertain; inspecting output does not release it.
    pane_running: Arc<RwLock<HashMap<String, String>>>,
    /// Process-default shell dialect; pane `current_command` may override at wrap time.
    shell_type: ShellType,
    tracking: TrackingConfig,
    events: broadcast::Sender<CommandEvent>,
    /// Wakes `wait_for` callers after durable commits.
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
            resource_uri: exec.resource_uri(),
            kind,
            status: exec.status,
        });
        self.notify.notify_waiters();
    }

    fn pane_key(pane_id: &str, socket: Option<&str>) -> String {
        format!(
            "{}|{}|{}",
            crate::targets::current().as_deref().unwrap_or(""),
            socket.unwrap_or(""),
            pane_id
        )
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
        // `Some(0)` must not fall into the slow per-character path.
        let delay_ms = delay_ms.filter(|delay| *delay > 0);

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
            target: crate::targets::current(),
            id: command_id.clone(),
            pane_id: pane_id.to_string(),
            socket: resolved_socket.clone(),
            command: command.to_string(),
            status: CommandStatus::Running,
            exit_code: None,
            output: if tracking_disabled {
                Some("Tracking disabled for raw_mode or no_enter commands".to_string())
            } else {
                None
            },
            // No marker-bounded pane capture exists yet.
            output_truncated: true,
            result_ready: false,
            reason: None,
            started_at: Instant::now(),
            completed_at: None,
            raw_mode,
            tracking_disabled,
        };

        let key = Self::pane_key(pane_id, resolved_socket.as_deref());
        if let Some(active_id) = self
            .pane_command_id(pane_id, resolved_socket.as_deref())
            .await
        {
            self.recover_completed(&active_id).await;
        }
        {
            let mut running = self.pane_running.write().await;
            if let Some(active_id) = running.get(&key) {
                return Err(Error::InvalidArgument {
                    message: format!(
                        "pane {pane_id} is busy with command {active_id}; do not retry in another window. If its state is uncertain, stop and report to the user"
                    ),
                });
            }
            running.insert(key, command_id.clone());
        }

        if tracking_disabled {
            self.active_commands
                .write()
                .await
                .insert(command_id.clone(), execution.clone());
            self.emit(CommandEventKind::Created, &execution);
            self.cleanup_completed().await;
            if let Err(error) = self
                .dispatch_keys(
                    pane_id,
                    command,
                    delay_ms,
                    no_enter,
                    resolved_socket.as_deref(),
                )
                .await
            {
                self.mark_dispatch_uncertain(&command_id, &error).await;
                return Err(error);
            }
            return Ok(command_id);
        }

        let secret = secret.expect("tracked mode always has a secret");
        let launch = TrackedLaunch {
            command_id: command_id.clone(),
            pane_id: pane_id.to_string(),
            command: command.to_string(),
            delay_ms,
            socket: resolved_socket.clone(),
            secret: secret.clone(),
        };

        self.active_commands
            .write()
            .await
            .insert(command_id.clone(), execution.clone());
        self.secrets
            .write()
            .await
            .insert(command_id.clone(), secret);
        self.emit(CommandEventKind::Created, &execution);
        self.cleanup_completed().await;

        if let Err(error) = self.start_tracked_launch(launch).await {
            self.mark_dispatch_uncertain(&command_id, &error).await;
            return Err(error);
        }

        Ok(command_id)
    }

    async fn start_tracked_launch(&self, launch: TrackedLaunch) -> Result<()> {
        run_tracked_launch(
            Arc::clone(&self.active_commands),
            Arc::clone(&self.secrets),
            Arc::clone(&self.pane_running),
            self.events.clone(),
            Arc::clone(&self.notify),
            self.tracking.clone(),
            self.shell_type,
            launch,
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
    ) -> Result<()> {
        if let Some(delay) = delay_ms.filter(|delay| *delay > 0) {
            for ch in wrapped_command.chars() {
                tmux::send_keys(pane_id, &ch.to_string(), true, socket).await?;
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if !no_enter {
                tmux::send_keys(pane_id, "Enter", false, socket).await?;
            }
        } else if no_enter {
            tmux::send_keys(pane_id, wrapped_command, false, socket).await?;
        } else {
            tmux::send_keys_with_enter(pane_id, wrapped_command, false, socket).await?;
        }
        Ok(())
    }

    async fn mark_dispatch_uncertain(&self, command_id: &str, error: &Error) {
        let mut commands = self.active_commands.write().await;
        if let Some(exec) = commands.get_mut(command_id) {
            exec.status = CommandStatus::TrackingError;
            exec.reason = Some(format!("Input delivery is uncertain: {error}. Stop and report to the user; do not retry, clear input, or switch windows to repeat the command."));
            exec.completed_at = Some(Instant::now());
            exec.result_ready = true;
            self.emit(CommandEventKind::Terminal, exec);
        }
    }

    /// Return the latest execution snapshot, reconciling a durable side-channel completion
    /// before using scrollback for partial output.
    #[allow(dead_code)] // Library API; MCP waits return cached state at their deadline.
    pub async fn check_status(
        &self,
        command_id: &str,
        socket_override: Option<&str>,
    ) -> Result<Option<CommandExecution>> {
        self.cleanup_completed().await;
        self.recover_completed(command_id).await;

        let mut execution = {
            let commands = self.active_commands.read().await;
            match commands.get(command_id) {
                Some(e) if e.target == crate::targets::current() => e.clone(),
                _ => return Ok(None),
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

    /// Return the current status after reconciling a durable completion, without
    /// capturing partial scrollback for a still-running command.
    pub async fn status_snapshot(&self, command_id: &str) -> Option<CommandExecution> {
        self.cleanup_completed().await;
        self.recover_completed(command_id).await;
        self.get_command(command_id).await
    }

    /// Block until the terminal result is ready or `wait_ms` elapses.
    ///
    /// Returns `(execution, wait_timed_out)`. Timeout does not change command status.
    pub async fn wait_for(
        &self,
        command_id: &str,
        wait_ms: u64,
    ) -> Result<Option<(CommandExecution, bool)>> {
        let waiting = async {
            self.recover_completed(command_id).await;
            loop {
                if let Some(exec) = self.get_command(command_id).await {
                    if exec.tracking_disabled {
                        return Ok(Some((exec, false)));
                    }
                    if exec.status.is_terminal() && exec.result_ready {
                        return Ok(Some((exec, false)));
                    }
                } else {
                    return Ok(None);
                }
                tokio::select! {
                    _ = self.notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        };
        match tokio::time::timeout(Duration::from_millis(wait_ms), waiting).await {
            Ok(result) => result,
            // A caller deadline does not cancel the remote command or trigger more IO.
            Err(_) => Ok(self.get_command(command_id).await.map(|exec| (exec, true))),
        }
    }

    /// Snapshot a tracked command, recovering a durable completion if its watcher missed it.
    pub async fn get_command(&self, id: &str) -> Option<CommandExecution> {
        let commands = self.active_commands.read().await;
        commands
            .get(id)
            .filter(|execution| execution.visible_to_current_target())
            .cloned()
    }

    /// Recover a terminal command when the long-lived `wait-for` watcher missed its signal.
    /// The private exit buffer is authoritative; a DONE marker must also be visible so a
    /// buffer left by a still-running stub/test cannot release the pane early.
    async fn recover_completed(&self, command_id: &str) -> bool {
        let (execution, secret) = {
            let commands = self.active_commands.read().await;
            let Some(execution) = commands.get(command_id).cloned() else {
                return false;
            };
            if execution.target != crate::targets::current() {
                return false;
            }
            if execution.status.is_terminal() || execution.tracking_disabled || execution.raw_mode {
                return execution.status.is_terminal();
            }
            let Some(secret) = self.secrets.read().await.get(command_id).cloned() else {
                return false;
            };
            (execution, secret)
        };

        let exit_code =
            match tmux::read_exit_code_buffer(&secret, execution.socket.as_deref()).await {
                Ok(exit_code) => exit_code,
                Err(_) => return false,
            };
        if !matches!(
            capture_bracketed_output(
                &execution.pane_id,
                &execution.id,
                self.tracking.capture_max_lines.max(1),
                execution.socket.as_deref(),
            )
            .await,
            Ok(BracketedOutput::Complete(_))
        ) {
            return false;
        }

        let checkpoint = Duration::from_secs(self.tracking.tracking_deadline_seconds)
            .max(Duration::from_secs(1));
        let captured = tokio::time::timeout(
            checkpoint,
            capture_terminal_output(
                &execution.pane_id,
                &execution.id,
                self.tracking.capture_max_lines.max(1),
                execution.socket.as_deref(),
            ),
        )
        .await
        .unwrap_or_else(|_| CapturedOutput::unavailable());
        let status = if exit_code == 0 {
            CommandStatus::Completed
        } else {
            CommandStatus::Failed
        };
        let snapshot = {
            let mut commands = self.active_commands.write().await;
            let mut secrets = self.secrets.write().await;
            let mut running = self.pane_running.write().await;
            let Some(stored) = commands.get_mut(command_id) else {
                return false;
            };
            if stored.status.is_terminal() {
                return true;
            }
            stored.status = status;
            stored.exit_code = Some(exit_code);
            stored.reason = None;
            stored.completed_at = Some(Instant::now());
            apply_captured_output(stored, captured);
            stored.result_ready = true;
            // Commit readiness and release together, with no cancellation point between them.
            secrets.remove(command_id);
            let key = Self::pane_key(&execution.pane_id, execution.socket.as_deref());
            if running.get(&key).is_some_and(|id| id == command_id) {
                running.remove(&key);
            }
            stored.clone()
        };
        self.emit(CommandEventKind::Terminal, &snapshot);
        let _ = tmux::delete_exit_code_buffer(&secret, execution.socket.as_deref()).await;
        true
    }

    /// Return the tracked command id occupying one pane.
    pub async fn pane_command_id(&self, pane_id: &str, socket: Option<&str>) -> Option<String> {
        let key = Self::pane_key(pane_id, socket);
        self.pane_running.read().await.get(&key).cloned()
    }

    /// Return one pane whose tracked command ended without a reliable completion signal.
    pub async fn uncertain_command(&self) -> Option<CommandExecution> {
        let mut command_ids = self
            .pane_running
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        command_ids.sort();
        let commands = self.active_commands.read().await;
        command_ids.into_iter().find_map(|id| {
            commands
                .get(&id)
                .filter(|exec| {
                    exec.target == crate::targets::current()
                        && exec.status == CommandStatus::TrackingError
                })
                .cloned()
        })
    }

    #[cfg(test)]
    pub(crate) async fn insert_test_execution(&self, execution: CommandExecution) {
        self.active_commands
            .write()
            .await
            .insert(execution.id.clone(), execution);
    }

    #[cfg(test)]
    pub(crate) async fn insert_test_occupant(&self, execution: CommandExecution) {
        let key = Self::pane_key(&execution.pane_id, execution.socket.as_deref());
        self.pane_running
            .write()
            .await
            .insert(key, execution.id.clone());
        self.insert_test_execution(execution).await;
    }

    #[cfg(test)]
    pub async fn insert_test_claim(&self, pane_id: &str, socket: Option<&str>, command_id: &str) {
        let key = Self::pane_key(pane_id, socket);
        self.pane_running
            .write()
            .await
            .insert(key, command_id.to_string());
    }

    /// List ids currently held in the tracker map.
    pub async fn get_active_ids(&self) -> Vec<String> {
        let commands = self.active_commands.read().await;
        commands
            .iter()
            .filter(|(_, execution)| execution.visible_to_current_target())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// True if a command record exists (including terminal until eviction).
    pub async fn has_command(&self, id: &str) -> bool {
        let commands = self.active_commands.read().await;
        commands
            .get(id)
            .is_some_and(|execution| execution.visible_to_current_target())
    }

    /// Drop every tracked entry bound to a pane on one tmux socket.
    ///
    /// Occupancy is keyed by `target|socket|pane_id` because pane ids are only unique
    /// within a tmux server.
    pub async fn purge_pane(&self, pane_id: &str, socket: Option<&str>) -> usize {
        let mut commands = self.active_commands.write().await;
        let mut secrets = self.secrets.write().await;
        let mut removed = 0usize;
        let ids: Vec<String> = commands
            .iter()
            .filter(|(_, e)| {
                e.target == crate::targets::current()
                    && e.pane_id == pane_id
                    && e.socket.as_deref() == socket
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(exec) = commands.remove(&id) {
                secrets.remove(&id);
                removed += 1;
                let _ = self.events.send(CommandEvent {
                    command_id: exec.id.clone(),
                    resource_uri: exec.resource_uri(),
                    kind: CommandEventKind::Evicted,
                    status: exec.status,
                });
            }
        }
        {
            let key = Self::pane_key(pane_id, socket);
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
        let occupied = self
            .pane_running
            .read()
            .await
            .values()
            .cloned()
            .collect::<HashSet<_>>();

        let mut evicted: Vec<CommandExecution> = Vec::new();
        {
            let mut commands = self.active_commands.write().await;
            let mut secrets = self.secrets.write().await;
            commands.retain(|id, exec| {
                let keep = if occupied.contains(id) {
                    true
                } else if !exec.status.is_terminal() {
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
                        if occupied.contains(id) {
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
                resource_uri: exec.resource_uri(),
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
    pane_running: Arc<RwLock<HashMap<String, String>>>,
    events: broadcast::Sender<CommandEvent>,
    notify: Arc<Notify>,
    tracking: TrackingConfig,
    shell_type: ShellType,
    launch: TrackedLaunch,
) -> Result<()> {
    let TrackedLaunch {
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
            resource_uri: exec.resource_uri(),
            kind: CommandEventKind::Updated,
            status: exec.status,
        });
        notify.notify_waiters();
    }

    dispatch_keys_free(&pane_id, &wrapped, delay_ms, socket.as_deref()).await?;

    spawn_side_channel_watcher(
        active_commands,
        secrets,
        pane_running,
        events,
        notify,
        tracking,
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
    pane_running: Arc<RwLock<HashMap<String, String>>>,
    events: broadcast::Sender<CommandEvent>,
    notify: Arc<Notify>,
    tracking: TrackingConfig,
    command_id: String,
    pane_id: String,
    socket: Option<String>,
    secret: String,
) {
    crate::targets::spawn(async move {
        let checkpoint =
            Duration::from_secs(tracking.tracking_deadline_seconds).max(Duration::from_secs(1));
        let channel = tmux::wait_signal_name(&secret);
        let wait_result = {
            let wait = tmux::wait_for_signal(&channel, socket.as_deref());
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    result = &mut wait => break result,
                    _ = tokio::time::sleep(checkpoint) => {
                        let still_tracked = active_commands
                            .read()
                            .await
                            .get(&command_id)
                            .is_some_and(|exec| !exec.status.is_terminal());
                        if !still_tracked {
                            return;
                        }
                        // `wait-for -S` is not a durable queue. Re-check the durable
                        // exit buffer so a lost watcher signal cannot strand the pane.
                        if tmux::read_exit_code_buffer(&secret, socket.as_deref())
                            .await
                            .is_ok()
                            && matches!(
                                capture_bracketed_output(
                                    &pane_id,
                                    &command_id,
                                    tracking.capture_max_lines.max(1),
                                    socket.as_deref(),
                                )
                                .await,
                                Ok(BracketedOutput::Complete(_))
                            )
                        {
                            break Ok(());
                        }
                    }
                }
            }
        };

        let (status, exit_code, reason, final_capture) = match wait_result {
            Ok(()) => match tmux::read_exit_code_buffer(&secret, socket.as_deref()).await {
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
            Err(e) => (
                CommandStatus::TrackingError,
                None,
                Some(format!("wait-for failed: {e}")),
                false,
            ),
        };

        // Commit lifecycle immediately, but keep the pane lease and public readiness
        // notification until the final bounded capture attempt has finished.
        let terminal_committed = {
            let mut commands = active_commands.write().await;
            commands.get_mut(&command_id).is_some_and(|exec| {
                if exec.status.is_terminal() {
                    return false;
                }
                exec.status = status;
                exec.exit_code = exit_code;
                exec.output_truncated = true;
                exec.result_ready = false;
                exec.reason = reason;
                exec.completed_at = Some(Instant::now());
                true
            })
        };

        secrets.write().await.remove(&command_id);
        let cleanup_socket = socket.clone();
        let cleanup_secret = secret.clone();
        crate::targets::spawn(async move {
            let _ = tmux::delete_exit_code_buffer(&cleanup_secret, cleanup_socket.as_deref()).await;
        });

        if terminal_committed {
            let max_lines = tracking.capture_max_lines.max(1);
            let capture = async {
                if final_capture {
                    capture_terminal_output(&pane_id, &command_id, max_lines, socket.as_deref())
                        .await
                } else {
                    let mut captured =
                        capture_running_output(&pane_id, &command_id, max_lines, socket.as_deref())
                            .await;
                    // A tracking error without the completion signal means the command's output
                    // boundary is unknown even when START remains visible.
                    captured.truncated = true;
                    captured
                }
            };
            let captured = tokio::time::timeout(checkpoint, capture)
                .await
                .unwrap_or_else(|_| CapturedOutput::unavailable());

            let ready_snapshot = {
                let mut commands = active_commands.write().await;
                commands.get_mut(&command_id).and_then(|exec| {
                    if !exec.status.is_terminal() {
                        return None;
                    }
                    apply_captured_output(exec, captured);
                    exec.result_ready = true;
                    Some(exec.clone())
                })
            };

            if status != CommandStatus::TrackingError {
                release_pane(&pane_running, &pane_id, socket.as_deref(), &command_id).await;
            }

            if let Some(exec) = ready_snapshot {
                let _ = events.send(CommandEvent {
                    command_id: exec.id.clone(),
                    resource_uri: exec.resource_uri(),
                    kind: CommandEventKind::Terminal,
                    status: exec.status,
                });
                notify.notify_waiters();
            }
        }
    });
}

async fn release_pane(
    pane_running: &RwLock<HashMap<String, String>>,
    pane_id: &str,
    socket: Option<&str>,
    command_id: &str,
) {
    let key = CommandTracker::pane_key(pane_id, socket);
    let mut running = pane_running.write().await;
    if running.get(&key).is_some_and(|active| active == command_id) {
        running.remove(&key);
    }
}

async fn dispatch_keys_free(
    pane_id: &str,
    wrapped_command: &str,
    delay_ms: Option<u64>,
    socket: Option<&str>,
) -> Result<()> {
    if let Some(delay) = delay_ms.filter(|delay| *delay > 0) {
        for ch in wrapped_command.chars() {
            tmux::send_keys(pane_id, &ch.to_string(), true, socket).await?;
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        tmux::send_keys(pane_id, "Enter", false, socket).await?;
    } else {
        tmux::send_keys_with_enter(pane_id, wrapped_command, false, socket).await?;
    }
    Ok(())
}

#[allow(dead_code)] // Used by the library's explicit refresh API.
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
        Ok(BracketedOutput::Complete(output)) => CapturedOutput {
            output: Some(output),
            truncated: false,
        },
        Ok(BracketedOutput::Open(output)) => CapturedOutput {
            output: Some(output),
            truncated: true,
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
                "echo \"{start_marker}\"; {command} ; set __tmux_mcp_ec $status; {tmux_bin} set-buffer -b {buf_q} -- $__tmux_mcp_ec; echo \"{END_MARKER_PREFIX}{command_id}_$__tmux_mcp_ec\"; {tmux_bin} wait-for -S {chan_q}"
            )
        }
        ShellType::Bash | ShellType::Unknown => {
            // A foreground SIGINT can discard the rest of an interactive Bash input
            // line, including a trap-protected loop. Finish at the next prompt in
            // that case; do not change INT traps or treat key delivery as completion.
            // Use per-command names so an old command cannot finalize a newer one.
            // ponytail: recovery needs the next prompt; shell exit/exec or commands
            // that replace PROMPT_COMMAND still require separate recovery handling.
            let saved = format!("__tmux_mcp_prompt_{secret}");
            let finish = format!("__tmux_mcp_finish_{secret}");
            let ec = format!("__tmux_mcp_ec_{secret}");
            let complete = format!(
                "{tmux_bin} set-buffer -b {buf_q} -- \"${ec}\"; echo \"{END_MARKER_PREFIX}{command_id}_${ec}\"; {tmux_bin} wait-for -S {chan_q}"
            );
            let finalize = tmux::shell_single_quote(&format!(
                "{ec}=${{{ec}-$?}}; {complete}; unset PROMPT_COMMAND; eval \"${saved}\"; unset {saved} {finish} {ec}"
            ));
            // Keep array syntax inside eval: sh/dash can use the legacy tail even
            // when configured as Bash. Preserve scalar/array prompt hooks verbatim.
            let prompt = tmux::shell_single_quote(&format!(
                "PROMPT_COMMAND=('eval \"${finish}\"' \"${{PROMPT_COMMAND[@]}}\")"
            ));
            format!(
                "if [ -n \"${{BASH_VERSION-}}\" ]; then {saved}=$(declare -p PROMPT_COMMAND 2>/dev/null); {finish}={finalize}; eval {prompt}; fi; echo \"{start_marker}\"; {command} ; {ec}=$?; if [ -n \"${{BASH_VERSION-}}\" ]; then eval \"${finish}\"; else {complete}; unset {ec}; fi"
            )
        }
        ShellType::Zsh => {
            format!(
                "echo \"{start_marker}\"; {command} ; __tmux_mcp_ec=$?; {tmux_bin} set-buffer -b {buf_q} -- \"$__tmux_mcp_ec\"; echo \"{END_MARKER_PREFIX}{command_id}_$__tmux_mcp_ec\"; {tmux_bin} wait-for -S {chan_q}"
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
/// Treats `&&`, `&>`, and redirections such as `2>&1` as non-background operators.
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
            '>' | '<' | '|'
                if !in_single_quote && !in_double_quote && iter.peek() == Some(&'&') =>
            {
                // File-descriptor redirections and Bash |& are not background jobs.
                iter.next();
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

    fn assert_snapshot_output_incomplete(exec: &CommandExecution) {
        assert!(exec.output_truncated);
        let snapshot = CommandSnapshot::from_execution(exec, None);
        assert!(snapshot.output_truncated);
        let wire = serde_json::to_value(snapshot).expect("serialize command snapshot");
        assert_eq!(wire["outputTruncated"], true);
    }

    fn running_execution(id: &str) -> CommandExecution {
        CommandExecution {
            target: None,
            id: id.to_string(),
            pane_id: "%1".to_string(),
            socket: None,
            command: "printf partial".to_string(),
            status: CommandStatus::Running,
            exit_code: None,
            output: None,
            output_truncated: false,
            result_ready: false,
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
        let mut tracking = TrackingConfig {
            capture_initial_lines: 10_000,
            capture_max_lines: 25,
            ..TrackingConfig::default()
        };
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

        let tracking = TrackingConfig {
            capture_initial_lines: 100,
            capture_max_lines: 2,
            ..TrackingConfig::default()
        };
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
        assert_snapshot_output_incomplete(&first);

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
            crate::targets::spawn(async move { tracker.check_status("cmd-race", None).await })
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

    #[tokio::test]
    async fn status_snapshot_does_not_capture_running_output() {
        let mut stub = TmuxStub::new();
        let capture_log = NamedTempFile::new().expect("capture log");
        stub.set_var("TMUX_STUB_CAPTURE_LOG", capture_log.path());
        let tracker = CommandTracker::new(ShellType::Bash);
        tracker
            .active_commands
            .write()
            .await
            .insert("cmd-snapshot".into(), running_execution("cmd-snapshot"));

        let snapshot = tracker
            .status_snapshot("cmd-snapshot")
            .await
            .expect("snapshot");
        assert_eq!(snapshot.status, CommandStatus::Running);
        assert!(
            !capture_log.path().exists()
                || std::fs::read_to_string(capture_log.path())
                    .expect("read capture log")
                    .is_empty()
        );
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
        assert!(
            wrapped.find("TMUX_MCP_DONE_cmd-1").unwrap() < wrapped.find("wait-for -S").unwrap()
        );
    }

    /// Real local shells with a shell-function tmux double: never contacts tmux/SSH.
    /// On Windows set TMUX_MCP_TEST_BASH to a Git Bash executable.
    #[tokio::test]
    async fn test_bash_interrupt_finalizes_once_and_restores_shell() {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        let bash = std::env::var_os("TMUX_MCP_TEST_BASH").unwrap_or_else(|| "bash".into());
        if cfg!(windows) && std::env::var_os("TMUX_MCP_TEST_BASH").is_none() {
            eprintln!("set TMUX_MCP_TEST_BASH to run the real Bash regression on Windows");
            return;
        }
        // With no controlling terminal, explicitly signal this test shell as well
        // as its child to model terminal SIGINT delivery without touching any peer.
        let interrupt = "bash --noprofile --norc -c 'kill -INT \"$PPID\"; kill -INT $$'";
        let cases = [
            ("true".to_string(), 0, "unset PROMPT_COMMAND; trap - INT"),
            ("false".to_string(), 1, "PROMPT_COMMAND=':'; trap ':' INT"),
            (
                "bash -c 'exit 130'".to_string(),
                130,
                "unset PROMPT_COMMAND",
            ),
            (
                interrupt.to_string(),
                130,
                "unset PROMPT_COMMAND; trap - INT",
            ),
            (interrupt.to_string(), 130, "PROMPT_COMMAND=':'; trap - INT"),
            (
                interrupt.to_string(),
                130,
                "PROMPT_COMMAND=':'; trap ':' INT",
            ),
            (
                "bash -c 'kill -INT \"$PPID\"; kill -INT \"$PPID\"; kill -INT $$'".to_string(),
                130,
                "PROMPT_COMMAND=(':' ':'); trap - INT",
            ),
            (
                format!("for x in 1 2; do {interrupt}; echo UNEXPECTED_LOOP_TAIL; done"),
                130,
                "PROMPT_COMMAND=(':' ':'); trap - INT",
            ),
            (
                "bash -c 'trap \"\" INT; kill -INT $$; echo CHILD_STILL_RUNNING; exit 0'"
                    .to_string(),
                0,
                "unset PROMPT_COMMAND; trap - INT",
            ),
            (
                "cd /; export TMUX_MCP_TEST_STATE=kept; false".to_string(),
                1,
                "PROMPT_COMMAND=(':' ':'); trap ':' INT",
            ),
        ];
        for (index, (command, exit_code, setup)) in cases.iter().enumerate() {
            let wrapped = wrap_tracked_command_side_channel(
                command,
                "interrupt-test",
                "abcdef",
                &ShellType::Bash,
                Some("/unused-test-socket"),
            );
            let script = format!(
                "PATH=/usr/bin:/bin; PS1=; PS2=\n\
                 tmux() {{ if [ \"$1\" = -S ]; then shift 2; fi; case \"$1\" in set-buffer) printf 'EXIT_CODE=%s\\n' \"${{@: -1}}\";; wait-for) echo COMPLETION_SIGNAL;; *) return 99;; esac; }}\n\
                 {setup}\n\
                 before_prompt=$(declare -p PROMPT_COMMAND 2>/dev/null); before_trap=$(trap -p INT)\n\
                 {wrapped}\n\
                 [ \"$(declare -p PROMPT_COMMAND 2>/dev/null)\" = \"$before_prompt\" ] && echo PROMPT_RESTORED\n\
                 [ \"$(trap -p INT)\" = \"$before_trap\" ] && echo TRAP_RESTORED\n\
                 [ -z \"${{__tmux_mcp_finish_abcdef+x}}${{__tmux_mcp_prompt_abcdef+x}}${{__tmux_mcp_ec_abcdef+x}}\" ] && echo STATE_CLEANED\n\
                 printf 'SHELL_STATE=%s:%s\\n' \"$PWD\" \"${{TMUX_MCP_TEST_STATE-}}\"\n\
                 exit 0\n"
            );
            let mut child = tokio::process::Command::new(&bash)
                .args(["--noprofile", "--norc", "-i"])
                .env_remove("BASH_ENV")
                .env_remove("ENV")
                .env_remove("PROMPT_COMMAND")
                .env("INPUTRC", "/dev/null")
                .env("TERM", "dumb")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("start local Bash");
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .await
                .unwrap();
            let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
                .await
                .expect("local shell must not hang")
                .expect("local shell output");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "case {index}: {stdout}\n{stderr}");
            for expected in [
                format!("EXIT_CODE={exit_code}"),
                "COMPLETION_SIGNAL".into(),
                "PROMPT_RESTORED".into(),
                "TRAP_RESTORED".into(),
                "STATE_CLEANED".into(),
            ] {
                assert_eq!(
                    stdout.lines().filter(|line| *line == expected).count(),
                    1,
                    "case {index}, expected exactly one {expected}: {stdout}\n{stderr}"
                );
            }
            assert!(
                !stdout.contains("UNEXPECTED_LOOP_TAIL"),
                "case {index}: {stdout}"
            );
            if command.contains("CHILD_STILL_RUNNING") {
                assert!(
                    stdout.find("CHILD_STILL_RUNNING").unwrap()
                        < stdout.find("COMPLETION_SIGNAL").unwrap()
                );
            }
            if command.contains("TMUX_MCP_TEST_STATE") {
                assert!(stdout.contains("SHELL_STATE=/:kept"), "{stdout}");
            }
        }
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
    async fn status_snapshot_recovers_completed_side_channel_without_watcher() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_EXIT_CODE", "7");
        let id = "orphaned-command";
        let secret = "orphaned-secret";
        let tracker = CommandTracker::new(ShellType::Bash);
        tracker.insert_test_occupant(running_execution(id)).await;
        tracker
            .secrets
            .write()
            .await
            .insert(id.to_string(), secret.to_string());
        stub.set_var(
            "TMUX_STUB_CAPTURE_OUTPUT",
            format!("TMUX_MCP_START_{id}\noutput\nTMUX_MCP_DONE_{id}_7\n"),
        );

        let snapshot = tracker.status_snapshot(id).await.expect("snapshot");
        assert_eq!(snapshot.status, CommandStatus::Failed);
        assert_eq!(snapshot.exit_code, Some(7));
        assert!(snapshot.result_ready);
        assert_eq!(tracker.pane_command_id("%1", None).await, None);
        assert!(
            tracker
                .execute_command("%1", "echo next", false, false, None, None)
                .await
                .is_ok(),
            "a recovered command must release its pane"
        );
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

        let (cmd, timed_out) = tracker
            .wait_for(&id, 5_000)
            .await
            .expect("wait")
            .expect("found");
        assert!(!timed_out);
        assert_eq!(cmd.status, CommandStatus::Completed);
        assert_eq!(cmd.exit_code, Some(0));
        assert_eq!(cmd.output.as_deref(), Some("final output"));
        assert!(!cmd.output_truncated);
        assert!(cmd.result_ready);
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

        let (cmd, timed_out) = tracker
            .wait_for(&id, 5_000)
            .await
            .expect("wait")
            .expect("found");
        assert!(!timed_out);
        assert_eq!(cmd.status, CommandStatus::Completed);
        assert_eq!(cmd.exit_code, Some(0));
        assert_eq!(cmd.output.as_deref(), Some("bounded tail"));
        assert!(cmd.output_truncated);
        assert!(cmd.result_ready);
        let logged = std::fs::read_to_string(capture_log.path()).expect("read capture log");
        assert_eq!(logged.lines().count(), FINAL_OUTPUT_CAPTURE_ATTEMPTS);
    }

    #[tokio::test]
    async fn final_capture_timeout_marks_result_ready_and_releases_pane() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_CAPTURE_SLEEP_SECS", "2");
        let tracker = CommandTracker::with_tracking(
            ShellType::Bash,
            TrackingConfig {
                tracking_deadline_seconds: 1,
                ..TrackingConfig::default()
            },
        );
        let id = tracker
            .execute_command("%1", "echo hi", false, false, None, None)
            .await
            .expect("execute");

        let (cmd, timed_out) = tracker
            .wait_for(&id, 3_000)
            .await
            .expect("wait")
            .expect("found");
        assert!(!timed_out);
        assert_eq!(cmd.status, CommandStatus::Completed);
        assert!(cmd.result_ready);
        assert!(cmd.output_truncated);
        assert!(tracker.pane_command_id("%1", None).await.is_none());
    }

    #[tokio::test]
    async fn execute_command_spoof_done_does_not_complete() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_WAIT_FOR_SLEEP_SECS", "2");
        stub.set_var("TMUX_STUB_EXIT_CODE_MISSING", "1");
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
    async fn execute_command_rejects_second_on_same_pane() {
        let tracker = CommandTracker::new(ShellType::Bash);
        let id = "running-command".to_string();
        tracker.insert_test_occupant(running_execution(&id)).await;
        let error = tracker
            .execute_command("%1", "echo two", false, false, None, None)
            .await
            .expect_err("second command must be rejected");

        assert!(error.to_string().contains("busy"));
        assert!(error.to_string().contains(&id));
        assert_eq!(tracker.get_active_ids().await, vec![id]);
    }

    #[tokio::test]
    async fn test_detach_cannot_bypass_occupied_pane() {
        let tracker = CommandTracker::new(ShellType::Bash);
        let mut execution = running_execution("detached");
        execution.raw_mode = true;
        execution.tracking_disabled = true;
        tracker.insert_test_occupant(execution).await;
        for detach in [false, true] {
            let error = tracker
                .execute_command("%1", "next", detach, false, None, None)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("busy"));
        }
        tracker
            .mark_dispatch_uncertain(
                "detached",
                &Error::InvalidArgument {
                    message: "connection lost".into(),
                },
            )
            .await;
        assert!(tracker.uncertain_command().await.is_some());
        assert_eq!(
            tracker.pane_command_id("%1", None).await.as_deref(),
            Some("detached")
        );
    }

    #[tokio::test]
    async fn tracking_error_keeps_pane_busy_after_inspection_and_retention() {
        let tracker = CommandTracker::with_tracking(
            ShellType::Bash,
            TrackingConfig {
                completed_retention_minutes: 0,
                ..TrackingConfig::default()
            },
        );
        let id = "uncertain-command".to_string();
        let mut failed = running_execution(&id);
        failed.status = CommandStatus::TrackingError;
        failed.completed_at = Some(Instant::now());
        tracker.insert_test_occupant(failed).await;
        tracker.cleanup_completed().await;
        assert_eq!(
            tracker
                .uncertain_command()
                .await
                .expect("uncertain command")
                .id,
            id
        );

        let error = tracker
            .execute_command("%1", "echo two", false, false, None, None)
            .await
            .expect_err("uncertain pane must remain busy");
        assert!(error.to_string().contains("busy"));

        tracker.status_snapshot(&id).await;
        tracker.cleanup_completed().await;
        assert!(tracker.uncertain_command().await.is_some());
        assert_eq!(
            tracker.pane_command_id("%1", None).await.as_deref(),
            Some(id.as_str())
        );
        assert!(tracker
            .execute_command("%1", "echo detached", true, false, None, None)
            .await
            .is_err());
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
    async fn caller_timeout_stays_running_after_tracking_checkpoint() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_WAIT_FOR_SLEEP_SECS", "3");
        stub.set_var("TMUX_STUB_EXIT_CODE_MISSING", "1");
        let tracker = CommandTracker::with_tracking(
            ShellType::Bash,
            TrackingConfig {
                tracking_deadline_seconds: 1,
                ..TrackingConfig::default()
            },
        );
        let id = tracker
            .execute_command("%1", "sleep 99", false, false, None, None)
            .await
            .expect("execute");
        let (cmd, timed_out) = tracker
            .wait_for(&id, 1_200)
            .await
            .expect("wait")
            .expect("found");
        assert!(timed_out);
        assert!(
            !cmd.status.is_terminal(),
            "elapsed waits must not terminalize; got {:?}",
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

    #[test]
    fn shell_redirections_are_not_background_operators() {
        for command in [
            "sleep 60&true",
            "sleep 1 &",
            "sleep 1&",
            "echo a&&sleep 1&echo b",
        ] {
            assert!(has_unquoted_shell_background_operator(command), "{command}");
        }
        for command in [
            "cmd 2>&1",
            "true && echo ok",
            "cmd &>log",
            "cmd 3<&0",
            "cmd |& cat",
            "echo 'a&b'",
            "echo \"a&b\"",
            r"echo a\&b",
        ] {
            assert!(
                !has_unquoted_shell_background_operator(command),
                "{command}"
            );
        }
    }

    #[tokio::test]
    async fn test_wait_budget_preserves_running_command_without_final_capture() {
        let tracker = CommandTracker::new(ShellType::Bash);
        tracker
            .insert_test_occupant(running_execution("budget"))
            .await;
        let (result, timing) = crate::timing::measure(tracker.wait_for("budget", 5)).await;
        let (execution, timed_out) = result.unwrap().unwrap();
        assert!(timed_out);
        assert_eq!(execution.status, CommandStatus::Running);
        assert!(!execution.result_ready);
        assert!(
            timing.transports.is_empty(),
            "deadline must not capture output"
        );
        assert_eq!(
            tracker.pane_command_id("%1", None).await.as_deref(),
            Some("budget")
        );
    }

    #[tokio::test]
    async fn test_target_isolates_identical_panes_results_and_purge() {
        let tracker = CommandTracker::new(ShellType::Bash);
        for target in ["server-admin", "server-intern"] {
            crate::targets::scope(target.into(), async {
                let mut execution = running_execution(target);
                execution.target = Some(target.into());
                tracker.insert_test_occupant(execution).await;
            })
            .await;
        }
        crate::targets::scope("server-admin".into(), async {
            assert_eq!(
                tracker.pane_command_id("%1", None).await.as_deref(),
                Some("server-admin")
            );
            assert!(tracker.get_command("server-intern").await.is_none());
            assert!(tracker
                .check_status("server-intern", None)
                .await
                .unwrap()
                .is_none());
            assert_eq!(tracker.purge_pane("%1", None).await, 1);
        })
        .await;
        crate::targets::scope("server-intern".into(), async {
            assert_eq!(
                tracker.pane_command_id("%1", None).await.as_deref(),
                Some("server-intern")
            );
            let record = tracker.get_command("server-intern").await.unwrap();
            assert_eq!(
                record.resource_uri(),
                "tmux://server-intern/command/server-intern/result"
            );
        })
        .await;
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
            logged.contains("=$?; if [ -n"),
            "bash pane should use POSIX exit status syntax, got: {logged}"
        );
        assert!(
            !logged.contains("set __tmux_mcp_ec $status"),
            "bash pane must not receive fish syntax, got: {logged}"
        );
        assert_eq!(
            logged.lines().count(),
            1,
            "payload and Enter should share one call"
        );
    }

    #[tokio::test]
    async fn execute_command_zero_delay_does_not_type_character_by_character() {
        let mut stub = TmuxStub::new();
        let log = NamedTempFile::new().expect("log");
        stub.set_var("TMUX_STUB_SEND_KEYS_LOG", log.path());
        let tracker = CommandTracker::new(ShellType::Bash);

        tracker
            .execute_command("%1", "true", false, false, Some(0), None)
            .await
            .expect("execute");

        let logged = std::fs::read_to_string(log.path()).expect("read log");
        assert_eq!(logged.lines().count(), 1);
    }

    #[tokio::test]
    async fn execute_command_send_failure_retains_uncertain_pane() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_ERROR_CMD", "send-keys");
        let tracker = CommandTracker::new(ShellType::Bash);
        let err = tracker
            .execute_command("%1", "echo hi", false, false, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Tmux { .. }));
        let uncertain = tracker
            .uncertain_command()
            .await
            .expect("retain uncertain input");
        assert_eq!(
            tracker.pane_command_id("%1", None).await.as_deref(),
            Some(uncertain.id.as_str())
        );
        assert!(uncertain.reason.unwrap().contains("Stop and report"));
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
        let id_b = tracker
            .execute_command("%1", "echo b", false, false, None, Some(socket_b.into()))
            .await
            .expect("execute on socket b");

        assert_eq!(tracker.purge_pane("%1", Some(socket_a)).await, 1);
        assert!(tracker.get_command(&id_a).await.is_none());
        assert!(tracker.get_command(&id_b).await.is_some());
        assert_eq!(tracker.purge_pane("%1", Some(socket_b)).await, 1);
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
        assert_eq!(
            tracker.pane_command_id("%1", None).await.as_deref(),
            Some(id.as_str())
        );
        assert!(tracker
            .execute_command("%1", "echo next", false, false, None, None)
            .await
            .is_err());
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
