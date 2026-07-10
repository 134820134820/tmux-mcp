//! Domain error taxonomy for policy, tmux process, and parse failures.
//!
//! MCP tools typically surface these as structured or text tool errors rather than
//! panicking; policy denials are intentional client-visible failures.

#![allow(dead_code)]

use thiserror::Error;

/// Convenience result type for tmux-mcp-rs operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Error variants returned across the library and MCP tool boundary.
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration file IO or TOML/schema failure (including invalid regex patterns).
    #[error("config error: {message}")]
    Config { message: String },

    /// Security policy denied the requested tool, target, path, or command.
    #[error("policy denied: {message}")]
    PolicyDenied { message: String },

    /// tmux (or SSH-wrapped tmux) process failed or returned a non-zero status.
    #[error("tmux error: {message}")]
    Tmux { message: String },

    /// Tabular or marker output from tmux could not be parsed into DTOs.
    #[error("parse error: {message}")]
    Parse { message: String },

    /// Caller-supplied arguments failed validation before any process spawn.
    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },
}
