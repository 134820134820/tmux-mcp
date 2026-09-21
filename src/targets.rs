//! SSH aliases are the target identities. Connections are scoped to async work.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, future::Future, path::Path};

use crate::errors::{Error, Result};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetsFile {
    pub targets: BTreeMap<String, TargetConfig>,
}

impl TargetsFile {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| Error::Config {
            message: format!("failed to read targets file: {e}"),
        })?;
        Self::parse(&content)
    }
    pub fn parse(content: &str) -> Result<Self> {
        let config: Self =
            toml::from_str(content.trim_start_matches('\u{feff}')).map_err(|e| Error::Config {
                message: format!("invalid targets file: {e}"),
            })?;
        if config.targets.is_empty() {
            return Err(Error::Config {
                message: "targets file must contain at least one target".into(),
            });
        }
        for name in config.targets.keys() {
            if name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
                || name.starts_with(['-', '.'])
            {
                return Err(Error::Config {
                    message: format!("invalid target name: {name:?}"),
                });
            }
        }
        Ok(config)
    }
}
tokio::task_local! { static CURRENT: String; }
pub fn current() -> Option<String> {
    CURRENT.try_with(Clone::clone).ok()
}
pub async fn scope<F: Future>(target: String, work: F) -> F::Output {
    CURRENT.scope(target, work).await
}
pub fn spawn<F>(work: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let target = current();
    tokio::spawn(async move {
        if let Some(target) = target {
            scope(target, work).await
        } else {
            work.await
        }
    })
}
pub fn uri_for(target: &str, uri: &str) -> String {
    uri.strip_prefix("tmux://")
        .filter(|rest| {
            ["command/", "pane/", "window/", "session/", "server/"]
                .iter()
                .any(|p| rest.starts_with(p))
        })
        .map_or_else(|| uri.to_string(), |rest| format!("tmux://{target}/{rest}"))
}
pub fn uri(uri: &str) -> String {
    current().map_or_else(|| uri.to_string(), |target| uri_for(&target, uri))
}
pub fn load_configured() -> Result<TargetsFile> {
    let path = std::env::var_os("TMUX_MCP_TARGETS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new("targets.toml").to_path_buf());
    TargetsFile::load(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_aliases() {
        assert!(TargetsFile::parse("[targets.x]\nnote='x'").is_ok());
        assert!(TargetsFile::parse("[targets.-bad]").is_err());
        assert!(TargetsFile::parse("[targets..bad]").is_err());
    }
}
