//! CLI entrypoint for the tmux-mcp-rs stdio MCP server.
//!
//! Loads optional `config.toml`, applies socket/SSH overrides via env, gates on
//! tmux 3.x when the version is detectable, then serves tools over stdio until
//! Ctrl-C / SIGTERM.

mod commands;
mod control;
mod errors;
mod gpu_monitor;
mod security;
mod server;
#[cfg(test)]
mod test_support;
mod tmux;
mod types;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rmcp::ServiceExt;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::commands::{CommandTracker, TrackingConfig};
use crate::control::{
    default_state_dir, load_or_create_token, validate_web_url, ControlClient, StatePaths,
};
use crate::security::{ConfigFile, SearchConfig, SecurityPolicy};
use crate::server::TmuxMcpServer;
use crate::types::ShellType;

/// Process CLI: shell dialect, config path, and socket/SSH isolation overrides.
#[derive(Parser, Debug)]
#[command(name = "tmux-mcp-rs")]
#[command(about = "Tmux MCP server in Rust")]
#[command(version)]
struct Cli {
    /// Default shell dialect for tracked-command wrappers (`bash`, `zsh`, `fish`).
    /// Overridden by `[shell].type` in config.toml when that field is set.
    #[arg(short = 't', long = "shell-type", default_value = "bash")]
    shell_type: String,

    /// Path to config.toml (security, tracking, search, shell, ssh sections).
    #[arg(short = 'c', long = "config")]
    config: Option<PathBuf>,

    /// Tmux socket path written to `TMUX_MCP_SOCKET` for process-wide isolation.
    #[arg(short = 's', long = "socket")]
    socket: Option<PathBuf>,

    /// Remote tmux via OpenSSH (`user@host` or extra ssh argv). Overrides config/env.
    #[arg(short = 'r', long = "ssh")]
    ssh: Option<String>,

    /// Run the local web control center instead of the stdio MCP server.
    #[arg(long)]
    web: bool,

    /// Loopback address for the local web control center.
    #[arg(long, default_value = "127.0.0.1:38473")]
    web_bind: SocketAddr,

    /// Local web control center used to record and authorize stdio MCP actions.
    #[arg(long)]
    web_url: Option<String>,

    /// Source name shown in the web control center (defaults to `mcp-<pid>`).
    #[arg(long)]
    client_name: Option<String>,

    /// Enable conversation-scoped Claude Channel notifications and GPU watch tools.
    #[arg(long)]
    claude_channel: bool,

    /// Expose advanced administrative tools; the default MCP surface is agent-core.
    #[arg(long)]
    full_tools: bool,
}

/// Map CLI/`config.toml` shell names to the tracker dialect (case-insensitive).
fn parse_shell_type(s: &str) -> Result<ShellType, String> {
    match s.to_lowercase().as_str() {
        "bash" => Ok(ShellType::Bash),
        "zsh" => Ok(ShellType::Zsh),
        "fish" => Ok(ShellType::Fish),
        _ => Err(format!(
            "unknown shell type \"{s}\", expected one of: bash, zsh, fish"
        )),
    }
}

fn init_tracing() {
    if let Ok(filter) = EnvFilter::try_from_default_env() {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .init();
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(_) => {
                let _ = ctrl_c.await;
                return;
            }
        };

        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Err(error) = crate::web::validate_bind_address(cli.web_bind) {
        eprintln!("Error parsing web bind address: {error}");
        std::process::exit(1);
    }
    if let Some(url) = &cli.web_url {
        if let Err(error) = validate_web_url(url) {
            eprintln!("Error parsing web URL: {error}");
            std::process::exit(1);
        }
    }
    if cli.web && cli.web_url.is_some() {
        eprintln!("Error: --web and --web-url select different process modes");
        std::process::exit(1);
    }

    if let Some(socket) = &cli.socket {
        std::env::set_var("TMUX_MCP_SOCKET", socket);
    }

    init_tracing();

    let (security_policy, config_shell_type, config_ssh, tracking_config, search_config) =
        match &cli.config {
            Some(path) => {
                let content = match std::fs::read_to_string(path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Error reading config file: {e}");
                        std::process::exit(1);
                    }
                };

                let config_file: ConfigFile = match toml::from_str(&content) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Error parsing config file: {e}");
                        std::process::exit(1);
                    }
                };

                let policy = match SecurityPolicy::load(path) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("Error loading security policy: {e}");
                        std::process::exit(1);
                    }
                };

                (
                    policy,
                    config_file.shell.shell_type,
                    config_file.ssh.remote,
                    config_file.tracking,
                    config_file.search,
                )
            }
            None => (
                match SecurityPolicy::default_with_env() {
                    Ok(policy) => policy,
                    Err(e) => {
                        eprintln!("Error loading security policy: {e}");
                        std::process::exit(1);
                    }
                },
                None,
                None,
                TrackingConfig::default(),
                SearchConfig::default(),
            ),
        };

    let shell_type_source = config_shell_type.as_deref().unwrap_or(&cli.shell_type);
    let shell_type = match parse_shell_type(shell_type_source) {
        Ok(shell_type) => shell_type,
        Err(e) => {
            eprintln!("Error parsing shell type: {e}");
            std::process::exit(1);
        }
    };

    let ssh_connection = cli.ssh.or(config_ssh).or_else(|| {
        std::env::var("TMUX_MCP_SSH")
            .ok()
            .filter(|ssh| !ssh.is_empty())
    });
    if let Some(ssh) = ssh_connection {
        match crate::tmux::parse_ssh_args(&ssh) {
            Ok(Some(_)) => std::env::set_var("TMUX_MCP_SSH", ssh),
            Ok(None) => std::env::remove_var("TMUX_MCP_SSH"),
            Err(e) => {
                eprintln!("Error parsing SSH connection string: {e}");
                std::process::exit(1);
            }
        }
    }

    if let Some((major, minor)) = crate::tmux::tmux_version().await {
        if major < 3 {
            eprintln!("Error: tmux-mcp-rs requires tmux 3.0 or newer; found tmux {major}.{minor}");
            std::process::exit(1);
        }
    }

    let state_paths = StatePaths::new(default_state_dir());
    if cli.web {
        let token = match load_or_create_token(&state_paths) {
            Ok(token) => token,
            Err(error) => {
                eprintln!("Error creating web control token: {error}");
                std::process::exit(1);
            }
        };
        let hub = match crate::web::HubState::open(state_paths) {
            Ok(hub) => hub,
            Err(error) => {
                eprintln!("Error opening web control state: {error}");
                std::process::exit(1);
            }
        };
        let socket = std::env::var("TMUX_MCP_SOCKET")
            .ok()
            .filter(|value| !value.is_empty());
        let tracker = Arc::new(if cli.config.is_some() {
            CommandTracker::with_tracking(shell_type, tracking_config)
        } else {
            CommandTracker::new(shell_type)
        });
        let app = crate::web::build_router(hub, token, security_policy, socket, tracker);
        let listener = match tokio::net::TcpListener::bind(cli.web_bind).await {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("Failed to bind web control center: {error}");
                std::process::exit(1);
            }
        };
        eprintln!("tmux MCP control center: http://{}", cli.web_bind);
        if let Err(error) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
        {
            eprintln!("Web control center error: {error}");
            std::process::exit(1);
        }
        return;
    }

    let tracker = if cli.config.is_some() {
        CommandTracker::with_tracking(shell_type, tracking_config)
    } else {
        CommandTracker::new(shell_type)
    };
    let control = match cli.web_url.as_deref() {
        Some(url) => {
            let source = cli
                .client_name
                .unwrap_or_else(|| format!("mcp-{}", std::process::id()));
            match ControlClient::new(url, source, state_paths) {
                Ok(control) => Some(control),
                Err(error) => {
                    eprintln!("Error creating web control client: {error}");
                    std::process::exit(1);
                }
            }
        }
        None => None,
    };
    let server = TmuxMcpServer::new_with_control_channel_and_exposure(
        tracker,
        security_policy,
        search_config,
        control,
        cli.claude_channel,
        cli.full_tools,
    );
    let gpu_monitor = server.gpu_monitor();

    tracing::info!("Starting tmux-mcp-rs server with stdio transport");

    let transport = rmcp::transport::io::stdio();

    match server.serve(transport).await {
        Ok(service) => {
            let cancel_token = service.cancellation_token();
            let mut wait = Box::pin(service.waiting());

            let server_error = tokio::select! {
                result = &mut wait => {
                    result.err()
                }
                _ = shutdown_signal() => {
                    cancel_token.cancel();
                    wait.await.err()
                }
            };
            gpu_monitor.shutdown().await;
            if let Some(error) = server_error {
                eprintln!("Server error: {error}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Failed to start server: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_shell_type;
    use crate::types::ShellType;

    #[test]
    fn parse_shell_type_recognizes_known_shells() {
        assert_eq!(parse_shell_type("bash"), Ok(ShellType::Bash));
        assert_eq!(parse_shell_type("zsh"), Ok(ShellType::Zsh));
        assert_eq!(parse_shell_type("fish"), Ok(ShellType::Fish));
    }

    #[test]
    fn parse_shell_type_is_case_insensitive() {
        assert_eq!(parse_shell_type("ZSH"), Ok(ShellType::Zsh));
    }

    #[test]
    fn parse_shell_type_rejects_unknown_shells() {
        let err = parse_shell_type("unknown").expect_err("unknown shell should fail");
        assert!(err.contains("unknown shell type \"unknown\""));
        assert!(err.contains("bash, zsh, fish"));
    }
}
