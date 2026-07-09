//! Marker-based command execution tracking for agent-driven shell work in panes.
//!
//! `CommandTracker` wraps user commands with START/DONE markers, sends them via
//! `send-keys`, then polls pane history (with capture backoff) until a DONE exit
//! code appears, markers scroll off past a deadline, or tracking is disabled
//! (`raw_mode` / `no_enter`).

use std::collections::HashMap;
#[cfg(test)]
use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Deserialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::errors::{Error, Result};
use crate::tmux;
use crate::types::{CommandExecution, CommandStatus, ShellType};

/// Prefix for the start marker, followed by command id.
pub const START_MARKER_PREFIX: &str = "TMUX_MCP_START_";

/// Prefix for the end marker, followed by command id and exit code.
pub const END_MARKER_PREFIX: &str = "TMUX_MCP_DONE_";

#[cfg(test)]
struct EnvVarGuard {
    key: &'static str,
    prev: Option<OsString>,
}

#[cfg(test)]
impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prev }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.prev.take() {
            std::env::set_var(self.key, prev);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

/// Capture backoff, completion retention, and expiry budgets for tracked commands.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackingConfig {
    #[serde(default = "default_capture_initial_lines")]
    pub capture_initial_lines: u32,
    #[serde(default = "default_capture_max_lines")]
    pub capture_max_lines: u32,
    #[serde(default = "default_capture_backoff_factor")]
    pub capture_backoff_factor: u32,
    #[serde(default = "default_completed_retention_minutes")]
    pub completed_retention_minutes: u64,
    #[serde(default = "default_completed_max_entries")]
    pub completed_max_entries: u32,
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

/// How long a command stays Pending after its START marker is no longer
/// reachable (scrolled past `capture_max_lines`) before being declared expired.
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

/// In-process registry of pending and recently completed pane commands.
#[derive(Debug)]
pub struct CommandTracker {
    active_commands: Arc<RwLock<HashMap<String, CommandExecution>>>,
    shell_type: ShellType,
    tracking: TrackingConfig,
}

impl CommandTracker {
    /// Build a tracker with default capture/retention budgets for `shell_type`.
    pub fn new(shell_type: ShellType) -> Self {
        Self::with_tracking(shell_type, TrackingConfig::default())
    }

    /// Build a tracker with caller-supplied capture/retention budgets.
    pub fn with_tracking(shell_type: ShellType, tracking: TrackingConfig) -> Self {
        Self {
            active_commands: Arc::new(RwLock::new(HashMap::new())),
            shell_type,
            tracking,
        }
    }

    /// Send a command into a pane, optionally wrapping it with START/DONE markers.
    ///
    /// Returns a command id for later `check_status` polls. Tracking is disabled
    /// for `raw_mode` or `no_enter` (markers would not complete reliably).
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

        if !raw_mode && !no_enter && command.contains(['\n', '\r']) {
            return Err(Error::InvalidArgument {
                message: "tracked commands cannot contain embedded newlines (\\n or \\r)"
                    .to_string(),
            });
        }
        if !raw_mode && !no_enter && has_unquoted_shell_comment_marker(command) {
            return Err(Error::InvalidArgument {
                message: "tracked commands cannot contain unquoted shell comment markers (#)"
                    .to_string(),
            });
        }
        if !raw_mode && !no_enter && has_unquoted_shell_background_operator(command) {
            return Err(Error::InvalidArgument {
                message: "tracked commands cannot contain unquoted shell background operators (&)"
                    .to_string(),
            });
        }

        let (wrapped_command, tracking_disabled) = if raw_mode || no_enter {
            (command.to_string(), true)
        } else {
            let marker_shell = self
                .marker_shell_type(pane_id, resolved_socket.as_deref())
                .await;
            let end_marker = get_end_marker(&marker_shell, &command_id);
            let start_marker = get_start_marker(&command_id);
            let wrapped = wrap_tracked_command(command, &start_marker, &end_marker);
            (wrapped, false)
        };

        let execution = CommandExecution {
            id: command_id.clone(),
            pane_id: pane_id.to_string(),
            socket: resolved_socket.clone(),
            command: command.to_string(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: if tracking_disabled {
                Some("Tracking disabled for raw_mode or no_enter commands".to_string())
            } else {
                None
            },
            started_at: Instant::now(),
            completed_at: None,
            raw_mode,
            tracking_disabled,
        };

        {
            let mut commands = self.active_commands.write().await;
            commands.insert(command_id.clone(), execution);
        }

        self.cleanup_completed().await;

        async fn rollback_on_send_error(
            active_commands: &Arc<RwLock<HashMap<String, CommandExecution>>>,
            command_id: &str,
            result: Result<()>,
        ) -> Result<()> {
            if result.is_err() {
                let mut commands = active_commands.write().await;
                commands.remove(command_id);
            }
            result
        }

        if let Some(delay) = delay_ms {
            for ch in wrapped_command.chars() {
                rollback_on_send_error(
                    &self.active_commands,
                    &command_id,
                    tmux::send_keys(pane_id, &ch.to_string(), true, resolved_socket.as_deref())
                        .await,
                )
                .await?;
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if !no_enter {
                rollback_on_send_error(
                    &self.active_commands,
                    &command_id,
                    tmux::send_keys(pane_id, "Enter", false, resolved_socket.as_deref()).await,
                )
                .await?;
            }
        } else {
            rollback_on_send_error(
                &self.active_commands,
                &command_id,
                tmux::send_keys(pane_id, &wrapped_command, false, resolved_socket.as_deref()).await,
            )
            .await?;
            if !no_enter {
                rollback_on_send_error(
                    &self.active_commands,
                    &command_id,
                    tmux::send_keys(pane_id, "Enter", false, resolved_socket.as_deref()).await,
                )
                .await?;
            }
        }

        Ok(command_id)
    }

    /// Poll pane history for markers and return the latest execution snapshot.
    ///
    /// Returns `None` if the id is unknown. Terminal statuses are sticky; Pending
    /// entries re-capture with line backoff until DONE, expiry, or retention cleanup.
    pub async fn check_status(
        &self,
        command_id: &str,
        socket_override: Option<&str>,
    ) -> Result<Option<CommandExecution>> {
        self.cleanup_completed().await;

        let execution = {
            let commands = self.active_commands.read().await;
            commands.get(command_id).cloned()
        };

        let mut execution = match execution {
            Some(e) => e,
            None => return Ok(None),
        };

        match execution.status {
            CommandStatus::Completed | CommandStatus::Error => {
                return Ok(Some(execution));
            }
            CommandStatus::Pending if execution.raw_mode || execution.tracking_disabled => {
                return Ok(Some(execution));
            }
            _ => {}
        }

        #[cfg(test)]
        let _env_guard = EnvVarGuard::set("TMUX_MCP_TEST_COMMAND_ID", &execution.id);

        let mut capture_lines = self.tracking.capture_initial_lines.max(1);
        let max_lines = self.tracking.capture_max_lines.max(capture_lines);
        let backoff = self.tracking.capture_backoff_factor.max(1);

        loop {
            let captured_output = tmux::capture_pane(
                &execution.pane_id,
                Some(capture_lines),
                false,
                None,
                None,
                true,
                execution.socket.as_deref().or(socket_override),
            )
            .await?;

            if let Some((output, exit_code)) = parse_command_output(&captured_output, &execution.id)
            {
                execution.exit_code = Some(exit_code);
                execution.output = Some(output);
                execution.completed_at = Some(Instant::now());
                execution.status = if exit_code == 0 {
                    CommandStatus::Completed
                } else {
                    CommandStatus::Error
                };

                let mut commands = self.active_commands.write().await;
                commands.insert(command_id.to_string(), execution.clone());
                break;
            }

            let start_visible = captured_output.contains(&get_start_marker(&execution.id));

            // DONE may still be outside the initial capture window, even when
            // START is visible. Widen before concluding the command is Pending.
            let widened = (capture_lines.saturating_mul(backoff)).min(max_lines);
            if widened > capture_lines {
                capture_lines = widened;
                continue;
            }

            // START visible but no DONE even at the widest capture: still
            // running. Stay Pending (caching an Error here would be sticky via
            // the early-return above).
            if start_visible {
                break;
            }

            // No marker even at the widest window: could be a high-output command
            // still running, or genuinely lost. Bound by time, not capture lines:
            // stay Pending until the command outlives the tracking deadline.
            let deadline = Duration::from_secs(self.tracking.tracking_deadline_seconds);
            if execution.started_at.elapsed() <= deadline {
                break;
            }

            execution.status = CommandStatus::Error;
            execution.output =
                Some("tracking expired; markers not found in pane history".to_string());
            execution.completed_at = Some(Instant::now());

            let mut commands = self.active_commands.write().await;
            commands.insert(command_id.to_string(), execution.clone());
            break;
        }

        self.cleanup_completed().await;

        Ok(Some(execution))
    }

    /// Snapshot a tracked command without re-capturing the pane.
    pub async fn get_command(&self, id: &str) -> Option<CommandExecution> {
        let commands = self.active_commands.read().await;
        commands.get(id).cloned()
    }

    /// List ids currently held in the tracker map (pending and retained completed).
    pub async fn get_active_ids(&self) -> Vec<String> {
        let commands = self.active_commands.read().await;
        commands.keys().cloned().collect()
    }

    /// Drop every tracked entry bound to `pane_id` (e.g. after the pane is killed).
    pub async fn purge_pane(&self, pane_id: &str) -> usize {
        let mut commands = self.active_commands.write().await;
        let before = commands.len();
        commands.retain(|_, exec| exec.pane_id != pane_id);
        before - commands.len()
    }

    async fn marker_shell_type(&self, pane_id: &str, socket: Option<&str>) -> ShellType {
        match tmux::pane_info(pane_id, socket).await {
            Ok(info) if info.current_command == "fish" => ShellType::Fish,
            _ => self.shell_type,
        }
    }

    /// Remove completed commands outside the configured retention window and count.
    async fn cleanup_completed(&self) {
        let retention_minutes = self.tracking.completed_retention_minutes;
        let retention_window = Duration::from_secs(retention_minutes.saturating_mul(60));
        let now = Instant::now();

        // Pending commands only transition when polled, so cleanup also reclaims
        // entries abandoned past the deadline plus retention window.
        let pending_abandon_window = Duration::from_secs(self.tracking.tracking_deadline_seconds)
            .saturating_add(retention_window)
            .max(Duration::from_secs(1));

        let mut commands = self.active_commands.write().await;
        commands.retain(|_, exec| {
            if exec.status == CommandStatus::Pending {
                let age = now
                    .checked_duration_since(exec.started_at)
                    .unwrap_or(Duration::ZERO);
                return age < pending_abandon_window;
            }
            let completed_at = match exec.completed_at {
                Some(instant) => instant,
                None => return true,
            };
            let age = now
                .checked_duration_since(completed_at)
                .unwrap_or(Duration::ZERO);
            age < retention_window
        });

        let max_entries = self.tracking.completed_max_entries as usize;
        if max_entries == 0 {
            return;
        }
        let mut completed: Vec<(String, Instant)> = commands
            .iter()
            .filter_map(|(id, exec)| {
                if exec.status == CommandStatus::Pending {
                    return None;
                }
                exec.completed_at
                    .map(|completed_at| (id.clone(), completed_at))
            })
            .collect();

        if completed.len() <= max_entries {
            return;
        }

        completed.sort_by_key(|(_, completed_at)| *completed_at);
        let excess = completed.len().saturating_sub(max_entries);
        for (id, _) in completed.into_iter().take(excess) {
            commands.remove(&id);
        }
    }
}

/// Get the end marker command for the given shell type.
///
/// Fish shell uses `$status` for exit codes, while bash/zsh use `$?`.
pub fn get_start_marker(command_id: &str) -> String {
    format!("{START_MARKER_PREFIX}{command_id}")
}

fn end_marker_prefix(command_id: &str) -> String {
    format!("{END_MARKER_PREFIX}{command_id}_")
}

/// Build the shell snippet that prints the DONE marker with exit code.
///
/// Fish uses `$status`; bash/zsh use `$?`.
pub fn get_end_marker(shell: &ShellType, command_id: &str) -> String {
    let prefix = end_marker_prefix(command_id);
    match shell {
        ShellType::Fish => format!("{prefix}$status"),
        ShellType::Bash | ShellType::Zsh | ShellType::Unknown => format!("{prefix}$?"),
    }
}

/// Wrap a tracked command with START/DONE markers.
///
/// A space precedes the DONE separator so a trailing backslash in `command`
/// cannot escape the semicolon (`\ ;` → escaped space + statement break).
fn wrap_tracked_command(command: &str, start_marker: &str, end_marker: &str) -> String {
    format!("echo \"{start_marker}\"; {command} ; echo \"{end_marker}\"")
}

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

fn has_unquoted_shell_background_operator(command: &str) -> bool {
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    let mut iter = command.chars().peekable();
    let mut prev_unquoted: Option<char> = None;

    while let Some(ch) = iter.next() {
        if escaped {
            escaped = false;
            prev_unquoted = Some(ch);
            continue;
        }

        match ch {
            '\\' if !in_single_quote => {
                escaped = true;
                prev_unquoted = Some(ch);
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
                prev_unquoted = Some(ch);
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
                prev_unquoted = Some(ch);
            }
            '&' if !in_single_quote && !in_double_quote => {
                let next = iter.peek().copied();
                if matches!(next, Some('&')) {
                    iter.next();
                    prev_unquoted = Some('&');
                    continue;
                }
                if matches!(next, Some('>')) {
                    prev_unquoted = Some(ch);
                    continue;
                }

                let prev_is_word = prev_unquoted
                    .map(|prev| !prev.is_whitespace() && !is_shell_separator_char(prev))
                    .unwrap_or(false);
                let next_is_word = next
                    .map(|next| !next.is_whitespace() && !is_shell_separator_char(next))
                    .unwrap_or(false);

                if !(prev_is_word && next_is_word) {
                    return true;
                }

                prev_unquoted = Some(ch);
            }
            _ => {
                prev_unquoted = Some(ch);
            }
        }
    }

    false
}

fn is_shell_separator_char(ch: char) -> bool {
    matches!(ch, ';' | '&' | '|' | '(' | ')' | '<' | '>')
}

/// Parse captured output to extract command output and exit code.
///
/// The DONE marker is authoritative for completion; START only delimits where
/// output begins and is optional (it may have scrolled off under heavy output).
/// Returns `None` if no DONE marker with a numeric exit code is present.
fn parse_command_output(captured: &str, command_id: &str) -> Option<(String, i32)> {
    let start_marker = get_start_marker(command_id);
    let after_start = match captured.rfind(&start_marker) {
        Some(start_idx) => &captured[start_idx + start_marker.len()..],
        None => captured,
    };

    // Find the LAST match of the end marker (not the first)
    // This is important because the pane output may contain the typed command line
    // (e.g., `echo TMUX_MCP_DONE_<id>_$?`) before the actual echoed output
    let end_prefix = end_marker_prefix(command_id);
    let end_regex = Regex::new(&format!(r"{}(\d+)", regex::escape(&end_prefix))).ok()?;
    let last_match = end_regex.captures_iter(after_start).last()?;

    let exit_code: i32 = last_match.get(1)?.as_str().parse().ok()?;

    let end_match = last_match.get(0)?;
    let output_end = end_match.start();

    let output = after_start[..output_end].trim().to_string();

    Some((output, exit_code))
}

/// Extract exit code from an end marker line.
#[allow(dead_code)]
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
    use crate::types::{CommandExecution, CommandStatus};
    use rstest::rstest;
    use std::time::Duration;
    use tempfile::tempdir;

    async fn assert_tracker_empty(tracker: &CommandTracker, context: &str) {
        assert!(
            tracker.get_active_ids().await.is_empty(),
            "{context} must not leave active command ids"
        );
        assert!(
            tracker.active_commands.read().await.is_empty(),
            "{context} must not leave stored commands"
        );
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
    #[case("TMUX_MCP_DONE_cmd-1_127", Some(127))]
    #[case("TMUX_MCP_DONE_cmd-1_255", Some(255))]
    #[case("some output TMUX_MCP_DONE_cmd-1_42 more text", Some(42))]
    #[case("no marker here", None)]
    #[case("TMUX_MCP_DONE_cmd-1_", None)]
    #[case("TMUX_MCP_DONE_cmd-1_abc", None)]
    fn test_extract_exit_code(#[case] input: &str, #[case] expected: Option<i32>) {
        assert_eq!(extract_exit_code(input, "cmd-1"), expected);
    }

    #[rstest]
    #[case(
        "prompt$ TMUX_MCP_START_cmd-1\nhello world\nTMUX_MCP_DONE_cmd-1_0\nprompt$",
        Some(("hello world".to_string(), 0))
    )]
    #[case(
        "TMUX_MCP_START_cmd-1\nerror occurred\nTMUX_MCP_DONE_cmd-1_1",
        Some(("error occurred".to_string(), 1))
    )]
    #[case(
        "old TMUX_MCP_START_cmd-1\nold output\nTMUX_MCP_DONE_cmd-1_0\nnew TMUX_MCP_START_cmd-1\nnew output\nTMUX_MCP_DONE_cmd-1_2",
        Some(("new output".to_string(), 2))
    )]
    #[case(
        "TMUX_MCP_START_cmd-1\nline1\nline2\nline3\nTMUX_MCP_DONE_cmd-1_0",
        Some(("line1\nline2\nline3".to_string(), 0))
    )]
    #[case("no markers at all", None)]
    #[case("TMUX_MCP_START_cmd-1\nno end marker", None)]
    // START scrolled off but DONE present: complete from DONE alone.
    #[case(
        "TMUX_MCP_DONE_cmd-1_0\nno start marker",
        Some((String::new(), 0))
    )]
    #[case(
        "earlier output lost\nfinal line\nTMUX_MCP_DONE_cmd-1_3",
        Some(("earlier output lost\nfinal line".to_string(), 3))
    )]
    fn test_parse_command_output(#[case] input: &str, #[case] expected: Option<(String, i32)>) {
        assert_eq!(parse_command_output(input, "cmd-1"), expected);
    }

    #[rstest]
    #[case(
        "$ echo TMUX_MCP_START_cmd-1\nTMUX_MCP_START_cmd-1\n$ ls -la\ntotal 0\ndrwxr-xr-x  2 user user  40 Jan  1 00:00 .\ndrwxr-xr-x 10 user user 200 Jan  1 00:00 ..\n$ echo TMUX_MCP_DONE_cmd-1_$?\nTMUX_MCP_DONE_cmd-1_0\n$",
        Some(0)
    )]
    #[case(
        "TMUX_MCP_START_cmd-1\ncommand not found: foobar\nTMUX_MCP_DONE_cmd-1_127",
        Some(127)
    )]
    fn test_parse_realistic_output(#[case] input: &str, #[case] expected_exit: Option<i32>) {
        let result = parse_command_output(input, "cmd-1");
        match (result, expected_exit) {
            (Some((_, code)), Some(expected)) => assert_eq!(code, expected),
            (None, None) => {}
            (result, expected) => panic!("Expected {expected:?}, got {result:?}"),
        }
    }

    #[test]
    fn test_markers_are_correct() {
        assert_eq!(START_MARKER_PREFIX, "TMUX_MCP_START_");
        assert_eq!(END_MARKER_PREFIX, "TMUX_MCP_DONE_");
        assert_eq!(get_start_marker("cmd-1"), "TMUX_MCP_START_cmd-1");
    }

    #[rstest]
    #[case(
        "grep foo",
        "echo \"TMUX_MCP_START_cmd-1\"; grep foo ; echo \"TMUX_MCP_DONE_cmd-1_$?\""
    )]
    #[case(
        "true",
        "echo \"TMUX_MCP_START_cmd-1\"; true ; echo \"TMUX_MCP_DONE_cmd-1_$?\""
    )]
    #[case(
        r"grep foo\",
        r#"echo "TMUX_MCP_START_cmd-1"; grep foo\ ; echo "TMUX_MCP_DONE_cmd-1_$?""#
    )]
    #[case(
        r"grep foo\\",
        r#"echo "TMUX_MCP_START_cmd-1"; grep foo\\ ; echo "TMUX_MCP_DONE_cmd-1_$?""#
    )]
    fn test_wrap_tracked_command_preserves_done_boundary(
        #[case] command: &str,
        #[case] expected: &str,
    ) {
        let wrapped = wrap_tracked_command(
            command,
            &get_start_marker("cmd-1"),
            &get_end_marker(&ShellType::Bash, "cmd-1"),
        );
        assert_eq!(wrapped, expected);
        assert!(
            wrapped.contains(" ; echo \""),
            "DONE separator must be preceded by a space: {wrapped}"
        );
        assert!(
            !wrapped.contains(r"\; echo"),
            "trailing backslash must not escape DONE separator: {wrapped}"
        );
    }

    #[rstest]
    #[case("grep pattern # notes")]
    #[case("# all comment")]
    #[case("true;# notes")]
    #[case("true && # notes")]
    #[case("(# subshell comment")]
    fn test_has_unquoted_shell_comment_marker_rejects_unquoted_hash(#[case] command: &str) {
        assert!(has_unquoted_shell_comment_marker(command));
    }

    #[rstest]
    #[case("echo before#after")]
    #[case("echo '# literal'")]
    #[case(r##"echo "# literal""##)]
    #[case(r"echo \# literal")]
    #[case(r#"printf "%s\n" "value # still data""#)]
    fn test_has_unquoted_shell_comment_marker_allows_quoted_or_escaped_hash(#[case] command: &str) {
        assert!(!has_unquoted_shell_comment_marker(command));
    }

    #[rstest]
    #[case("sleep 60 &")]
    #[case("sleep 60&")]
    #[case("true; & echo bad")]
    #[case("(sleep 60 &)")]
    fn test_has_unquoted_shell_background_operator_rejects_background_ampersand(
        #[case] command: &str,
    ) {
        assert!(has_unquoted_shell_background_operator(command));
    }

    #[rstest]
    #[case("echo 'R&D'")]
    #[case(r#"echo "R&D""#)]
    #[case(r"echo R\&D")]
    #[case("echo R&D")]
    #[case("true && echo ok")]
    #[case("echo hi &> out.txt")]
    fn test_has_unquoted_shell_background_operator_allows_data_ampersand(#[case] command: &str) {
        assert!(!has_unquoted_shell_background_operator(command));
    }

    #[tokio::test]
    async fn execute_command_trailing_backslash_wraps_with_unescapable_done_boundary() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command("%1", r"grep foo\", false, false, None, None)
            .await
            .expect("execute command with trailing backslash");

        assert!(!id.is_empty());
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(
            log.contains(r"grep foo\ ; echo"),
            "send-keys payload must guard DONE with space before semicolon, got: {log}"
        );
        assert!(
            !log.contains(r"grep foo\; echo"),
            "trailing backslash must not escape DONE separator, got: {log}"
        );
    }

    #[tokio::test]
    async fn execute_command_uses_fish_done_marker_when_pane_is_fish() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        stub.set_var(
            "TMUX_STUB_PANE_INFO_OUTPUT",
            "%1\t@1\t%1\t1\tpane-one\t/tmp\tfish\t80\t24\t1234\t0",
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command("%1", "echo hi", false, false, None, None)
            .await
            .expect("execute command");

        assert!(!id.is_empty());
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(
            log.contains("$status"),
            "fish pane should use fish DONE marker, got: {log}"
        );
        assert!(
            !log.contains("$?"),
            "fish pane should not use bash DONE marker, got: {log}"
        );
    }

    #[tokio::test]
    async fn execute_command_uses_configured_done_marker_when_pane_is_not_fish() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        stub.set_var(
            "TMUX_STUB_PANE_INFO_OUTPUT",
            "%1\t@1\t%1\t1\tpane-one\t/tmp\tzsh\t80\t24\t1234\t0",
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command("%1", "echo hi", false, false, None, None)
            .await
            .expect("execute command");

        assert!(!id.is_empty());
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(
            log.contains("$?"),
            "non-fish pane should use configured DONE marker, got: {log}"
        );
        assert!(
            !log.contains("$status"),
            "non-fish pane should not force fish DONE marker, got: {log}"
        );
    }

    #[tokio::test]
    async fn execute_command_falls_back_to_configured_marker_when_pane_info_fails() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        stub.set_var("TMUX_STUB_ERROR_CMD", "display-message");
        let tracker = CommandTracker::new(ShellType::Fish);

        let id = tracker
            .execute_command("%1", "echo hi", false, false, None, None)
            .await
            .expect("execute command despite pane-info failure");

        assert!(!id.is_empty());
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(
            log.contains("$status"),
            "pane-info failure should fall back to configured marker, got: {log}"
        );
    }

    #[rstest]
    fn test_command_tracker_new() {
        let tracker = CommandTracker::new(ShellType::Bash);
        assert!(matches!(tracker.shell_type, ShellType::Bash));
    }

    #[tokio::test]
    async fn execute_command_with_delay_sends_enter() {
        let _stub = TmuxStub::new();
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command("%1", "echo hi", false, false, Some(0), None)
            .await
            .expect("execute command");

        assert!(!id.is_empty());
    }

    #[tokio::test]
    async fn execute_command_without_delay_sends_enter() {
        let _stub = TmuxStub::new();
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command("%1", "echo hi", false, false, None, None)
            .await
            .expect("execute command");

        assert!(!id.is_empty());
    }

    #[tokio::test]
    async fn execute_command_returns_error_when_send_keys_fails() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_ERROR_CMD", "send-keys");
        let tracker = CommandTracker::new(ShellType::Bash);

        let err = tracker
            .execute_command("%1", "echo hi", false, false, None, None)
            .await
            .unwrap_err();

        match err {
            Error::Tmux { message } => assert!(message.contains("stub error")),
            _ => panic!("expected tmux error"),
        }

        assert_tracker_empty(&tracker, "failed dispatch").await;
    }

    #[tokio::test]
    async fn execute_command_with_delay_rolls_back_when_send_keys_fails() {
        let mut stub = TmuxStub::new();
        stub.set_var("TMUX_STUB_ERROR_CMD", "send-keys");
        let tracker = CommandTracker::new(ShellType::Bash);

        let err = tracker
            .execute_command("%1", "echo hi", false, false, Some(0), None)
            .await
            .unwrap_err();

        match err {
            Error::Tmux { message } => assert!(message.contains("stub error")),
            _ => panic!("expected tmux error"),
        }

        assert_tracker_empty(&tracker, "delayed send failure").await;
    }

    #[tokio::test]
    async fn execute_command_rejects_embedded_newline_in_tracked_mode() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let err = tracker
            .execute_command("%1", "echo a\necho b", false, false, None, None)
            .await
            .unwrap_err();

        match err {
            Error::InvalidArgument { message } => {
                assert!(message.contains("newline") || message.contains("\\n"));
            }
            _ => panic!("expected InvalidArgument, got {err:?}"),
        }

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(log.is_empty(), "send_keys should not be called, got: {log}");
    }

    #[tokio::test]
    async fn execute_command_rejects_unquoted_hash_comment_in_tracked_mode() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let err = tracker
            .execute_command("%1", "grep pattern # notes", false, false, None, None)
            .await
            .unwrap_err();

        match err {
            Error::InvalidArgument { message } => {
                assert!(message.contains("unquoted shell comment markers (#)"));
            }
            _ => panic!("expected InvalidArgument, got {err:?}"),
        }

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(log.is_empty(), "send_keys should not be called, got: {log}");
    }

    #[tokio::test]
    async fn execute_command_rejects_unquoted_background_operator_in_tracked_mode() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let err = tracker
            .execute_command("%1", "sleep 60 &", false, false, None, None)
            .await
            .unwrap_err();

        match err {
            Error::InvalidArgument { message } => {
                assert!(message.contains("unquoted shell background operators (&)"));
            }
            _ => panic!("expected InvalidArgument, got {err:?}"),
        }

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(log.is_empty(), "send_keys should not be called, got: {log}");
    }

    #[tokio::test]
    async fn execute_command_allows_quoted_escaped_and_intraword_ampersand_in_tracked_mode() {
        for command in [r#"echo "R&D""#, r"echo R\&D", "echo R&D"] {
            let mut stub = TmuxStub::new();
            let temp_dir = tempdir().expect("tempdir");
            let log_path = temp_dir.path().join("send-keys.log");
            stub.set_var(
                "TMUX_STUB_SEND_KEYS_LOG",
                log_path.to_str().expect("log path"),
            );
            let tracker = CommandTracker::new(ShellType::Bash);

            let id = tracker
                .execute_command("%1", command, false, false, None, None)
                .await
                .expect("ampersand used as data should be allowed");

            assert!(!id.is_empty());
            let log = std::fs::read_to_string(&log_path).expect("read log");
            assert!(
                log.contains(command),
                "tracked ampersand data command should be sent, got: {log}"
            );
        }
    }

    #[tokio::test]
    async fn execute_command_allows_quoted_hash_in_tracked_mode() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command(
                "%1",
                r##"echo "# not a comment""##,
                false,
                false,
                None,
                None,
            )
            .await
            .expect("quoted hash should be allowed");

        assert!(!id.is_empty());
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(
            log.contains(r##"echo "# not a comment" ; echo"##),
            "tracked quoted hash command should be sent, got: {log}"
        );
    }

    #[tokio::test]
    async fn execute_command_allows_background_operator_in_raw_mode() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command("%1", "sleep 60 &", true, false, None, None)
            .await
            .expect("raw mode should allow background operators");

        assert!(!id.is_empty());
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(
            log.contains("sleep 60 &"),
            "raw mode should send command as provided, got: {log}"
        );
    }

    #[tokio::test]
    async fn execute_command_allows_background_operator_in_no_enter_mode() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command("%1", "sleep 60 &", false, true, None, None)
            .await
            .expect("no_enter mode should allow background operators");

        assert!(!id.is_empty());
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(
            log.contains("sleep 60 &"),
            "no_enter mode should send command as provided, got: {log}"
        );
    }

    #[tokio::test]
    async fn execute_command_rejects_embedded_carriage_return_in_tracked_mode() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let err = tracker
            .execute_command("%1", "echo a\recho b", false, false, None, None)
            .await
            .unwrap_err();

        match err {
            Error::InvalidArgument { message } => {
                assert!(message.contains("newline") || message.contains("\\r"));
            }
            _ => panic!("expected InvalidArgument, got {err:?}"),
        }

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(log.is_empty(), "send_keys should not be called, got: {log}");
    }

    #[tokio::test]
    async fn execute_command_allows_newline_in_raw_mode() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command("%1", "echo a\necho b", true, false, None, None)
            .await
            .expect("raw mode should allow newlines");

        assert!(!id.is_empty());
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(!log.is_empty(), "raw mode should send keys");
    }

    #[tokio::test]
    async fn execute_command_allows_hash_in_raw_mode() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command("%1", "grep pattern # notes", true, false, None, None)
            .await
            .expect("raw mode should allow hash comments");

        assert!(!id.is_empty());
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(
            log.contains("grep pattern # notes"),
            "raw mode should send command as provided, got: {log}"
        );
    }

    #[tokio::test]
    async fn execute_command_allows_hash_in_no_enter_mode() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command("%1", "grep pattern # notes", false, true, None, None)
            .await
            .expect("no_enter mode should allow hash comments");

        assert!(!id.is_empty());
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(
            log.contains("grep pattern # notes"),
            "no_enter mode should send command as provided, got: {log}"
        );
    }

    #[tokio::test]
    async fn execute_command_allows_newline_in_no_enter_mode() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let log_path = temp_dir.path().join("send-keys.log");
        stub.set_var(
            "TMUX_STUB_SEND_KEYS_LOG",
            log_path.to_str().expect("log path"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);

        let id = tracker
            .execute_command("%1", "echo a\necho b", false, true, None, None)
            .await
            .expect("no_enter mode should allow newlines");

        assert!(!id.is_empty());
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(!log.is_empty(), "no_enter mode should send keys");
    }

    #[tokio::test]
    async fn execute_command_keeps_new_pending_with_zero_cleanup_window() {
        let _stub = TmuxStub::new();
        let tracking = TrackingConfig {
            completed_retention_minutes: 0,
            tracking_deadline_seconds: 0,
            ..TrackingConfig::default()
        };
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);

        let id = tracker
            .execute_command("%1", "echo hi", false, false, None, None)
            .await
            .expect("execute command");

        assert!(tracker.get_command(&id).await.is_some());
    }

    #[tokio::test]
    async fn check_status_returns_early_for_completed() {
        let tracker = CommandTracker::new(ShellType::Bash);
        let id = "completed-cmd".to_string();
        let execution = CommandExecution {
            id: id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "echo done".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            output: Some("done".into()),
            started_at: Instant::now(),
            completed_at: Some(Instant::now()),
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(id.clone(), execution);
        }

        let result = tracker.check_status(&id, None).await.expect("check status");
        assert!(matches!(
            result.map(|cmd| cmd.status),
            Some(CommandStatus::Completed)
        ));
    }

    #[tokio::test]
    async fn check_status_returns_none_for_unknown_id() {
        let tracker = CommandTracker::new(ShellType::Bash);
        let result = tracker
            .check_status("missing-command", None)
            .await
            .expect("check status");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn purge_pane_removes_only_matching_pane_entries() {
        let tracker = CommandTracker::new(ShellType::Bash);

        for (id, pane_id) in [("matching-1", "%1"), ("other", "%2"), ("matching-2", "%1")] {
            let execution = CommandExecution {
                id: id.into(),
                pane_id: pane_id.into(),
                socket: None,
                command: format!("echo {id}"),
                status: CommandStatus::Pending,
                exit_code: None,
                output: None,
                started_at: Instant::now(),
                completed_at: None,
                raw_mode: false,
                tracking_disabled: false,
            };

            let mut commands = tracker.active_commands.write().await;
            commands.insert(id.into(), execution);
        }

        assert_eq!(tracker.purge_pane("%1").await, 2);
        assert!(tracker.get_command("matching-1").await.is_none());
        assert!(tracker.get_command("matching-2").await.is_none());
        assert!(tracker.get_command("other").await.is_some());
    }

    #[tokio::test]
    async fn check_status_sets_error_on_nonzero_exit() {
        let mut stub = TmuxStub::new();
        let id = "error-cmd".to_string();
        stub.set_var(
            "TMUX_STUB_CAPTURE_OUTPUT",
            format!("TMUX_MCP_START_{id}\nbad\nTMUX_MCP_DONE_{id}_1\n"),
        );
        let tracker = CommandTracker::new(ShellType::Bash);
        let execution = CommandExecution {
            id: id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "false".into(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: None,
            started_at: Instant::now(),
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(id.clone(), execution);
        }

        let result = tracker.check_status(&id, None).await.expect("check status");
        let status = result.map(|cmd| cmd.status).unwrap();
        assert_eq!(status, CommandStatus::Error);
    }

    #[tokio::test]
    async fn check_status_retries_capture_until_markers_found() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let count_path = temp_dir.path().join("capture-count");
        let id = "retry-cmd".to_string();

        stub.set_var(
            "TMUX_STUB_CAPTURE_COUNT_FILE",
            count_path.to_str().expect("count path"),
        );
        stub.set_var("TMUX_STUB_CAPTURE_AFTER", "2");
        stub.set_var("TMUX_STUB_CAPTURE_BEFORE", "prompt\nno markers yet\n");
        stub.set_var(
            "TMUX_STUB_CAPTURE_AFTER_OUTPUT",
            format!("TMUX_MCP_START_{id}\nretry ok\nTMUX_MCP_DONE_{id}_0\n"),
        );

        let tracking = TrackingConfig {
            capture_initial_lines: 2,
            capture_max_lines: 4,
            capture_backoff_factor: 2,
            ..TrackingConfig::default()
        };
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);
        let execution = CommandExecution {
            id: id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "echo retry".into(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: None,
            started_at: Instant::now(),
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(id.clone(), execution);
        }

        let result = tracker.check_status(&id, None).await.expect("check status");
        let command = result.expect("command");
        assert_eq!(command.status, CommandStatus::Completed);
        assert_eq!(command.exit_code, Some(0));
        assert_eq!(command.output.as_deref(), Some("retry ok"));

        let count = std::fs::read_to_string(&count_path)
            .expect("read count")
            .trim()
            .parse::<u32>()
            .expect("parse count");
        assert!(count >= 2);
    }

    #[tokio::test]
    async fn check_status_widens_when_start_visible_without_done() {
        let mut stub = TmuxStub::new();
        let temp_dir = tempdir().expect("tempdir");
        let count_path = temp_dir.path().join("capture-count");
        let id = "start-visible-widen-cmd".to_string();

        stub.set_var(
            "TMUX_STUB_CAPTURE_COUNT_FILE",
            count_path.to_str().expect("count path"),
        );
        stub.set_var("TMUX_STUB_CAPTURE_AFTER", "2");
        stub.set_var(
            "TMUX_STUB_CAPTURE_BEFORE",
            format!("TMUX_MCP_START_{id}\npartial output\n"),
        );
        stub.set_var(
            "TMUX_STUB_CAPTURE_AFTER_OUTPUT",
            format!("TMUX_MCP_START_{id}\nwidened ok\nTMUX_MCP_DONE_{id}_0\n"),
        );

        let tracking = TrackingConfig {
            capture_initial_lines: 2,
            capture_max_lines: 8,
            capture_backoff_factor: 2,
            ..TrackingConfig::default()
        };
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);
        let execution = CommandExecution {
            id: id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "echo widened".into(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: None,
            started_at: Instant::now(),
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(id.clone(), execution);
        }

        let command = tracker
            .check_status(&id, None)
            .await
            .expect("check status")
            .expect("command");
        assert_eq!(command.status, CommandStatus::Completed);
        assert_eq!(command.exit_code, Some(0));
        assert_eq!(command.output.as_deref(), Some("widened ok"));

        let count = std::fs::read_to_string(&count_path)
            .expect("read count")
            .trim()
            .parse::<u32>()
            .expect("parse count");
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn check_status_sets_error_when_markers_never_found() {
        let mut stub = TmuxStub::new();
        let id = "expired-cmd".to_string();
        stub.set_var("TMUX_STUB_CAPTURE_OUTPUT", "no markers here");

        // deadline 0: markers-never-found resolves to a terminal Error immediately.
        let tracking = TrackingConfig {
            capture_initial_lines: 1,
            capture_max_lines: 2,
            capture_backoff_factor: 2,
            tracking_deadline_seconds: 0,
            ..TrackingConfig::default()
        };
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);
        let execution = CommandExecution {
            id: id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "echo missing".into(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: None,
            started_at: Instant::now(),
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(id.clone(), execution);
        }

        let result = tracker.check_status(&id, None).await.expect("check status");
        let command = result.expect("command");
        assert_eq!(command.status, CommandStatus::Error);
        assert_eq!(
            command.output.as_deref(),
            Some("tracking expired; markers not found in pane history")
        );
    }

    #[tokio::test]
    async fn check_status_stays_pending_when_start_lost_within_deadline() {
        // START lost but well within the deadline: must stay Pending (not Error)
        // so it can still complete once DONE appears.
        let mut stub = TmuxStub::new();
        let id = "lost-start-cmd".to_string();
        stub.set_var("TMUX_STUB_CAPTURE_OUTPUT", "buried output, no markers");

        let tracking = TrackingConfig {
            capture_initial_lines: 1,
            capture_max_lines: 2,
            capture_backoff_factor: 2,
            tracking_deadline_seconds: 600,
            ..TrackingConfig::default()
        };
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);
        let execution = CommandExecution {
            id: id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "seq 1 100000".into(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: None,
            started_at: Instant::now(),
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(id.clone(), execution);
        }

        let command = tracker
            .check_status(&id, None)
            .await
            .expect("check status")
            .expect("command");
        assert_eq!(command.status, CommandStatus::Pending);
        let stored = tracker.get_command(&id).await.expect("stored command");
        assert_eq!(stored.status, CommandStatus::Pending);
    }

    #[tokio::test]
    async fn check_status_completes_when_start_marker_scrolled_off() {
        // DONE is authoritative: a visible DONE completes the command even if
        // START scrolled out of the window.
        let mut stub = TmuxStub::new();
        let id = "done-only-cmd".to_string();
        stub.set_var(
            "TMUX_STUB_CAPTURE_OUTPUT",
            format!("...truncated output...\nlast line\nTMUX_MCP_DONE_{id}_0\n"),
        );

        let tracker = CommandTracker::new(ShellType::Bash);
        let execution = CommandExecution {
            id: id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "seq 1 100000".into(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: None,
            started_at: Instant::now(),
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(id.clone(), execution);
        }

        let command = tracker
            .check_status(&id, None)
            .await
            .expect("check status")
            .expect("command");
        assert_eq!(command.status, CommandStatus::Completed);
        assert_eq!(command.exit_code, Some(0));
    }

    #[tokio::test]
    async fn check_status_terminates_with_degenerate_backoff_factor() {
        // Regression: capture_backoff_factor == 1 cannot widen the window; the
        // loop must still terminate (fall through to expiry) rather than spin.
        let mut stub = TmuxStub::new();
        let id = "degenerate-backoff-cmd".to_string();
        stub.set_var("TMUX_STUB_CAPTURE_OUTPUT", "no markers here");

        let tracking = TrackingConfig {
            capture_initial_lines: 1,
            capture_max_lines: 16,
            capture_backoff_factor: 1,
            tracking_deadline_seconds: 0,
            ..TrackingConfig::default()
        };
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);
        let execution = CommandExecution {
            id: id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "echo missing".into(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: None,
            started_at: Instant::now(),
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(id.clone(), execution);
        }

        let command = tracker
            .check_status(&id, None)
            .await
            .expect("check status")
            .expect("command");
        assert_eq!(command.status, CommandStatus::Error);
    }

    #[tokio::test]
    async fn check_status_stays_pending_while_command_still_running() {
        // Regression: START visible, DONE not yet printed (still running) must
        // NOT be cached as a terminal Error (the early-return makes it sticky).
        let mut stub = TmuxStub::new();
        let id = "running-cmd".to_string();
        stub.set_var(
            "TMUX_STUB_CAPTURE_OUTPUT",
            format!("prompt\nTMUX_MCP_START_{id}\nstill working...\n"),
        );

        let tracking = TrackingConfig {
            capture_initial_lines: 1,
            capture_max_lines: 2,
            capture_backoff_factor: 2,
            ..TrackingConfig::default()
        };
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);
        let execution = CommandExecution {
            id: id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "sleep 1".into(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: None,
            started_at: Instant::now(),
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(id.clone(), execution);
        }

        // First poll: START seen, DONE absent -> still Pending, not Error.
        let first = tracker
            .check_status(&id, None)
            .await
            .expect("check status")
            .expect("command");
        assert_eq!(first.status, CommandStatus::Pending);
        assert!(first.output.is_none());

        // The map entry must remain Pending so a later poll can still complete.
        let stored = tracker.get_command(&id).await.expect("stored command");
        assert_eq!(stored.status, CommandStatus::Pending);
    }

    #[tokio::test]
    async fn cleanup_completed_removes_old_and_preserves_pending() {
        let tracking = TrackingConfig {
            completed_retention_minutes: 1,
            completed_max_entries: 1000,
            ..TrackingConfig::default()
        };
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);
        let old_id = "old".to_string();
        let new_id = "new".to_string();
        let pending_id = "pending".to_string();

        let now = Instant::now();
        let old_exec = CommandExecution {
            id: old_id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "old".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            output: Some("old".into()),
            started_at: now,
            completed_at: Some(now - Duration::from_secs(120)),
            raw_mode: false,
            tracking_disabled: false,
        };
        let new_exec = CommandExecution {
            id: new_id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "new".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            output: Some("new".into()),
            started_at: now,
            completed_at: Some(now),
            raw_mode: false,
            tracking_disabled: false,
        };
        let pending_exec = CommandExecution {
            id: pending_id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "pending".into(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: None,
            started_at: now,
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(old_id.clone(), old_exec);
            commands.insert(new_id.clone(), new_exec);
            commands.insert(pending_id.clone(), pending_exec);
        }

        tracker.cleanup_completed().await;

        let commands = tracker.active_commands.read().await;
        assert!(!commands.contains_key(&old_id));
        assert!(commands.contains_key(&new_id));
        assert!(commands.contains_key(&pending_id));
    }

    #[tokio::test]
    async fn cleanup_completed_trims_to_max_entries() {
        let tracking = TrackingConfig {
            completed_retention_minutes: 10,
            completed_max_entries: 2,
            ..TrackingConfig::default()
        };
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);
        let oldest_id = "oldest".to_string();
        let middle_id = "middle".to_string();
        let newest_id = "newest".to_string();
        let pending_id = "pending".to_string();

        let now = Instant::now();
        let oldest_exec = CommandExecution {
            id: oldest_id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "oldest".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            output: Some("oldest".into()),
            started_at: now,
            completed_at: Some(now - Duration::from_secs(180)),
            raw_mode: false,
            tracking_disabled: false,
        };
        let middle_exec = CommandExecution {
            id: middle_id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "middle".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            output: Some("middle".into()),
            started_at: now,
            completed_at: Some(now - Duration::from_secs(120)),
            raw_mode: false,
            tracking_disabled: false,
        };
        let newest_exec = CommandExecution {
            id: newest_id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "newest".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            output: Some("newest".into()),
            started_at: now,
            completed_at: Some(now - Duration::from_secs(60)),
            raw_mode: false,
            tracking_disabled: false,
        };
        let pending_exec = CommandExecution {
            id: pending_id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "pending".into(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: None,
            started_at: now,
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(oldest_id.clone(), oldest_exec);
            commands.insert(middle_id.clone(), middle_exec);
            commands.insert(newest_id.clone(), newest_exec);
            commands.insert(pending_id.clone(), pending_exec);
        }

        tracker.cleanup_completed().await;

        let commands = tracker.active_commands.read().await;
        assert!(!commands.contains_key(&oldest_id));
        assert!(commands.contains_key(&middle_id));
        assert!(commands.contains_key(&newest_id));
        assert!(commands.contains_key(&pending_id));
    }

    #[tokio::test]
    async fn cleanup_completed_zero_max_entries_keeps_fresh_completed_within_retention() {
        let tracking = TrackingConfig {
            completed_retention_minutes: 10,
            completed_max_entries: 0,
            ..TrackingConfig::default()
        };
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);
        let id = "fresh-completed".to_string();
        let now = Instant::now();
        let execution = CommandExecution {
            id: id.clone(),
            pane_id: "%1".into(),
            socket: None,
            command: "echo fresh".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            output: Some("fresh".into()),
            started_at: now,
            completed_at: Some(now),
            raw_mode: false,
            tracking_disabled: false,
        };

        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert(id.clone(), execution);
        }

        tracker.cleanup_completed().await;

        let command = tracker
            .get_command(&id)
            .await
            .expect("fresh completed command should remain within retention");
        assert_eq!(command.status, CommandStatus::Completed);
        assert_eq!(command.output.as_deref(), Some("fresh"));
    }

    #[tokio::test]
    async fn cleanup_drops_abandoned_pending_but_keeps_recent() {
        // A Pending command transitions only when polled. cleanup must reclaim
        // ones abandoned past (deadline + retention) so a fire-and-forget client
        // cannot leak entries forever, while keeping recently-started ones.
        let tracking = TrackingConfig {
            tracking_deadline_seconds: 1,
            completed_retention_minutes: 0,
            ..TrackingConfig::default()
        };
        let tracker = CommandTracker::with_tracking(ShellType::Bash, tracking);

        let mk = |id: &str, started: Instant| CommandExecution {
            id: id.into(),
            pane_id: "%1".into(),
            socket: None,
            command: "sleep 100".into(),
            status: CommandStatus::Pending,
            exit_code: None,
            output: None,
            started_at: started,
            completed_at: None,
            raw_mode: false,
            tracking_disabled: false,
        };
        let stale_started = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("backdate started_at");
        {
            let mut commands = tracker.active_commands.write().await;
            commands.insert("stale".into(), mk("stale", stale_started));
            commands.insert("fresh".into(), mk("fresh", Instant::now()));
        }

        tracker.cleanup_completed().await;

        let ids = tracker.get_active_ids().await;
        assert!(
            !ids.contains(&"stale".to_string()),
            "abandoned pending should be reclaimed"
        );
        assert!(
            ids.contains(&"fresh".to_string()),
            "recently-started pending should be kept"
        );
    }

    #[tokio::test]
    async fn concurrent_execute_command_tracks_unique_ids() {
        // Invariant: concurrent execute_command calls each get a distinct id and
        // none are lost from active_commands (the RwLock serializes the inserts).
        let _stub = TmuxStub::new();
        let tracker = Arc::new(CommandTracker::new(ShellType::Bash));

        let mut handles = Vec::new();
        for i in 0..16 {
            let tracker = Arc::clone(&tracker);
            handles.push(tokio::spawn(async move {
                tracker
                    .execute_command(&format!("%{i}"), "echo hi", false, false, None, None)
                    .await
                    .expect("execute command")
            }));
        }

        let mut ids = Vec::new();
        for handle in handles {
            ids.push(handle.await.expect("join task"));
        }

        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "command ids must be unique");
        let tracked = tracker.get_active_ids().await;
        for id in &ids {
            assert!(tracked.contains(id), "command {id} must be tracked");
        }
    }
}
