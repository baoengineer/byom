use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Main model slug; empty picks the first model in the account catalog.
    pub model: String,
    /// Model for Claude Code's background work; empty picks a small catalog model.
    pub small_model: String,
    pub provider: String,
    pub upstream_base_url: String,
    /// Context window Claude Code compacts against; 0 uses the catalog value.
    pub context_tokens: u64,
    /// `auto` (WebSocket with HTTP fallback) or `http`.
    pub transport: String,
    /// Claude model whose client-side handling Claude Code applies to GPT models.
    pub behaves_as: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: String::new(),
            small_model: String::new(),
            provider: "openai".to_owned(),
            upstream_base_url: "https://api.openai.com/v1".to_owned(),
            context_tokens: 0,
            transport: "auto".to_owned(),
            behaves_as: "claude-opus-5-5".to_owned(),
        }
    }
}

/// Resolve local state without creating directories or reading credentials.
pub fn state_dir() -> Result<PathBuf> {
    resolve_state_dir(env::var_os("BYOCLAUDE_HOME"), env::var_os("HOME"))
}

fn resolve_state_dir(state_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(path) = state_home {
        if path.is_empty() {
            bail!("BYOCLAUDE_HOME must not be empty");
        }
        return Ok(PathBuf::from(path));
    }
    let home = home
        .filter(|path| !path.is_empty())
        .context("cannot resolve state directory: set BYOCLAUDE_HOME or HOME")?;
    Ok(PathBuf::from(home).join(".byoclaude-rs"))
}

/// Read only the local config.json. A missing file uses defaults; other errors do not.
pub fn load() -> Result<Config> {
    load_from_path(&state_dir()?.join("config.json"))
}

fn load_from_path(path: &Path) -> Result<Config> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    if !value.is_object() {
        bail!("configuration in {} must be a JSON object", path.display());
    }
    let config: Config =
        serde_json::from_value(value).with_context(|| format!("parsing {}", path.display()))?;
    config
        .validate()
        .with_context(|| format!("invalid configuration in {}", path.display()))?;
    Ok(config)
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.model.trim() != self.model || self.small_model.trim() != self.small_model {
            bail!("model names must not have surrounding whitespace");
        }
        if self.provider != "openai" {
            bail!("provider must be openai");
        }
        if !matches!(self.transport.as_str(), "auto" | "http") {
            bail!("transport must be auto or http");
        }
        if self.behaves_as.trim().is_empty() {
            bail!("behaves_as must name a Claude model");
        }
        // URL parsing is local; HTTP also supports loopback mock upstreams.
        let url = reqwest::Url::parse(&self.upstream_base_url)
            .context("upstream_base_url must be an absolute HTTP or HTTPS URL")?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            bail!("upstream_base_url must be an absolute HTTP or HTTPS URL");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!("upstream_base_url must not contain credentials");
        }
        if url.query().is_some() || url.fragment().is_some() {
            bail!("upstream_base_url must not contain a query or fragment");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_json(json: &str) -> Result<Config> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.json");
        fs::write(&path, json)?;
        load_from_path(&path)
    }

    #[test]
    fn defaults_match_contract() {
        let config = Config::default();
        assert_eq!(config.model, "");
        assert_eq!(config.provider, "openai");
        assert_eq!(config.upstream_base_url, "https://api.openai.com/v1");
        assert_eq!(config.transport, "auto");
        config.validate().unwrap();
    }

    #[test]
    fn state_directory_override_and_fallback() {
        assert_eq!(
            resolve_state_dir(Some("/custom/state".into()), Some("/home/user".into())).unwrap(),
            PathBuf::from("/custom/state")
        );
        assert_eq!(
            resolve_state_dir(None, Some("/home/user".into())).unwrap(),
            PathBuf::from("/home/user/.byoclaude-rs")
        );
        assert!(resolve_state_dir(Some("".into()), Some("/home/user".into())).is_err());
        assert!(resolve_state_dir(None, None).is_err());
        assert!(resolve_state_dir(None, Some("".into())).is_err());
    }

    #[test]
    fn absent_file_defaults_without_creating_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("absent");
        assert_eq!(
            load_from_path(&state.join("config.json")).unwrap(),
            Config::default()
        );
        assert!(!state.exists());
    }

    #[test]
    fn partial_config_uses_defaults() {
        let config = read_json(r#"{"model":"custom-model"}"#).unwrap();
        assert_eq!(config.model, "custom-model");
        assert_eq!(config.provider, "openai");
        assert_eq!(read_json("{}").unwrap(), Config::default());
    }

    #[test]
    fn loads_complete_local_config() {
        let config = read_json(r#"{"model":"local","provider":"openai","upstream_base_url":"http://127.0.0.1:1234/v1","context_tokens":8192}"#).unwrap();
        assert_eq!(config.model, "local");
        assert_eq!(config.upstream_base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(config.context_tokens, 8192);
    }

    #[test]
    fn rejects_malformed_unknown_and_invalid_settings() {
        for json in [
            "",
            "{",
            "null",
            "[]",
            r#"{"modle":"typo"}"#,
            r#"{"model":"  "}"#,
            r#"{"provider":"unsupported"}"#,
            r#"{"transport":"carrier-pigeon"}"#,
            r#"{"context_tokens":-1}"#,
            r#"{"context_tokens":"200000"}"#,
            r#"{"upstream_base_url":"/v1"}"#,
            r#"{"upstream_base_url":"ftp://example.com/v1"}"#,
            r#"{"upstream_base_url":"https://user:secret@example.com/v1"}"#,
            r#"{"upstream_base_url":"https://example.com/v1?key=secret"}"#,
            r#"{"upstream_base_url":"https://example.com/v1#fragment"}"#,
        ] {
            assert!(read_json(json).is_err(), "accepted {json:?}");
        }
    }

    #[test]
    fn io_errors_do_not_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_from_path(dir.path()).is_err());
    }
}
