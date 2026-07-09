//! Shared DTOs for tmux topology, paste buffers, buffer search, and tracked commands.
//!
//! These types cross the MCP tool/resource boundary as JSON (camelCase fields where
//! noted) and also back in-process tracking state that is not serialized.

#![allow(dead_code)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Instant;

/// tmux session summary returned by list/find tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub attached: bool,
    pub windows: u32,
}

/// tmux window summary returned by list tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Window {
    pub id: String,
    pub name: String,
    pub active: bool,
    pub session_id: String,
}

/// tmux pane summary returned by list tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Pane {
    pub id: String,
    pub window_id: String,
    pub active: bool,
    pub title: String,
}

/// Detailed pane metadata (cwd, command, size, pid) for targeting and layout tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PaneInfo {
    pub id: String,
    pub window_id: String,
    pub session_id: String,
    pub title: String,
    pub active: bool,
    pub current_path: String,
    pub current_command: String,
    pub width: u32,
    pub height: u32,
    pub pid: Option<u32>,
    pub in_mode: bool,
}

/// Detailed window metadata (layout, zoom, active pane) for focus and layout tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowInfo {
    pub id: String,
    pub name: String,
    pub session_id: String,
    pub active: bool,
    pub layout: String,
    pub panes: u32,
    pub width: u32,
    pub height: u32,
    pub zoomed: bool,
    pub active_pane_id: String,
}

/// Attached tmux client (TTY/session/pid) used for observer-aware operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClientInfo {
    pub tty: String,
    pub name: String,
    pub session_name: String,
    pub pid: Option<u32>,
    pub attached: bool,
}

/// Paste-buffer listing entry with size and creation metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BufferInfo {
    pub name: String,
    pub size: u32,
    #[serde(rename = "sizeBytes")]
    pub size_bytes: u64,
    #[serde(rename = "orderIndex")]
    pub order_index: u32,
    pub created: Option<i64>,
}

/// Match strategy for `search-buffer` / `subsearch-buffer` tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    Literal,
    Regex,
}

/// One buffer search hit with byte offsets, context window, and optional similarity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BufferSearchMatch {
    #[serde(rename = "matchId")]
    pub match_id: String,
    pub buffer: String,
    #[serde(rename = "offsetBytes")]
    pub offset_bytes: u64,
    #[serde(rename = "matchLen")]
    pub match_len: u32,
    #[serde(rename = "contextStart")]
    pub context_start: u64,
    #[serde(rename = "contextEnd")]
    pub context_end: u64,
    pub snippet: String,
    pub similarity: Option<f32>,
}

/// Structured result of a multi-buffer or anchor-scoped buffer search.
///
/// Includes scan budgets, truncation/resume cursors, and optional fuzzy stats so
/// clients can page large buffers without re-scanning completed ranges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BufferSearchOutput {
    pub query: String,
    pub mode: SearchMode,
    #[serde(rename = "contextBytes")]
    pub context_bytes: u32,
    #[serde(rename = "maxMatches")]
    pub max_matches: u32,
    #[serde(rename = "includeSimilarity")]
    pub include_similarity: bool,
    #[serde(rename = "fuzzyMatch")]
    pub fuzzy_match: bool,
    #[serde(rename = "similarityThreshold")]
    pub similarity_threshold: Option<f32>,
    pub buffers: Vec<String>,
    #[serde(rename = "totalMatches")]
    pub total_matches: u32,
    #[serde(rename = "buffersScanned")]
    pub buffers_scanned: u32,
    #[serde(rename = "bytesScannedTotal")]
    pub bytes_scanned_total: u64,
    #[serde(rename = "truncatedBuffers")]
    pub truncated_buffers: Vec<String>,
    #[serde(rename = "resumeFromOffset")]
    pub resume_from_offset: BTreeMap<String, u64>,
    pub matches: Vec<BufferSearchMatch>,
    #[serde(rename = "maxSimilarity")]
    pub max_similarity: Option<f32>,
    #[serde(rename = "avgSimilarity")]
    pub avg_similarity: Option<f32>,
    #[serde(rename = "fuzzySkippedLines")]
    pub fuzzy_skipped_lines: u32,
    #[serde(rename = "fuzzySkippedBytes")]
    pub fuzzy_skipped_bytes: u64,
}

/// Window node in a session tree snapshot (window plus its panes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowTree {
    pub window: Window,
    pub panes: Vec<Pane>,
}

/// Session tree snapshot used by session resources and multi-pane planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionTree {
    pub session: Session,
    pub windows: Vec<WindowTree>,
}

/// Shell dialect used when wrapping tracked commands with START/DONE markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum ShellType {
    #[default]
    Bash,
    Zsh,
    Fish,
    Unknown,
}

/// Lifecycle status of a tracked command execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CommandStatus {
    Pending,
    Completed,
    Error,
}

/// In-memory record of a command sent to a pane, including tracking markers state.
///
/// Not serialized on the wire as-is; MCP tools project selected fields into tool output.
#[derive(Debug, Clone)]
pub struct CommandExecution {
    pub id: String,
    pub pane_id: String,
    pub socket: Option<String>,
    pub command: String,
    pub status: CommandStatus,
    pub exit_code: Option<i32>,
    pub output: Option<String>,
    pub started_at: Instant,
    pub completed_at: Option<Instant>,
    pub raw_mode: bool,
    pub tracking_disabled: bool,
}
