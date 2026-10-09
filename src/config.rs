use std::collections::BTreeMap;
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
    /// Main model ID; empty picks the first model in the catalog.
    pub model: String,
    /// Model for Claude Code's background work (titles, summaries); empty picks a small model.
    #[serde(alias = "small_model")]
    pub background: String,
    /// Default model for subagents that do not name one; empty leaves Claude Code's default.
    pub subagent: String,
    /// Model IDs that Claude Code's opus/sonnet/haiku aliases resolve to.
    pub aliases: Aliases,
    /// Relay Claude models to Anthropic on Claude Code's own sign-in; off until the user opts in.
    pub relay: bool,
    pub upstream_base_url: String,
    /// Context window Claude Code compacts against; 0 uses the catalog value.
    pub context_tokens: u64,
    /// `auto` (WebSocket with HTTP fallback) or `http`.
    pub transport: String,
    /// Claude model whose client-side handling Claude Code applies to other models.
    pub behaves_as: String,
    /// Turn on Claude Code's agent teams, so teammates can run on any model.
    pub teams: bool,
    /// Provider settings and user-defined providers, keyed by provider ID.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Models to try, in order, when a model fails before producing output.
    pub fallbacks: BTreeMap<String, Vec<String>>,
    /// Accepted from 0.1.0 configs and ignored.
    #[serde(rename = "provider", skip_serializing)]
    #[doc(hidden)]
    pub legacy_provider: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Aliases {
    pub opus: String,
    pub sonnet: String,
    pub haiku: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub name: String,
    /// `anthropic`, `openai-chat` or `openai-responses`; empty uses the built-in value.
    pub protocol: String,
    pub base_url: String,
    /// A key, `$ENV_VAR`, or `!command` printing the key.
    pub api_key: String,
    /// Model IDs to offer when the provider has no usable model list.
    pub models: Vec<String>,
    pub disabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: String::new(),
            background: String::new(),
            subagent: String::new(),
            aliases: Aliases::default(),
            relay: false,
            upstream_base_url: "https://api.openai.com/v1".to_owned(),
            context_tokens: 0,
            transport: "auto".to_owned(),
            behaves_as: "claude-opus-5-5".to_owned(),
            teams: false,
            providers: BTreeMap::new(),
            fallbacks: BTreeMap::new(),
            legacy_provider: None,
        }
    }
}

fn check_url(name: &str, value: &str) -> Result<()> {
    // URL parsing is local; HTTP also supports loopback and local servers.
    let url = reqwest::Url::parse(value)
        .with_context(|| format!("{name} must be an absolute HTTP or HTTPS URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("{name} must be an absolute HTTP or HTTPS URL");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("{name} must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("{name} must not contain a query or fragment");
    }
    Ok(())
}

/// Resolve local state without creating directories or reading credentials.
pub fn state_dir() -> Result<PathBuf> {
    resolve_state_dir(env::var_os("BYOM_HOME"), env::var_os("HOME"))
}

fn resolve_state_dir(state_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(path) = state_home {
        if path.is_empty() {
            bail!("BYOM_HOME must not be empty");
        }
        return Ok(PathBuf::from(path));
    }
    let home = home
        .filter(|path| !path.is_empty())
        .context("cannot resolve state directory: set BYOM_HOME or HOME")?;
    Ok(PathBuf::from(home).join(".byom"))
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
        let names = [
            &self.model,
            &self.background,
            &self.subagent,
            &self.aliases.opus,
            &self.aliases.sonnet,
            &self.aliases.haiku,
        ];
        if names.iter().any(|n| n.trim() != n.as_str()) {
            bail!("model names must not have surrounding whitespace");
        }
        if self
            .legacy_provider
            .as_deref()
            .is_some_and(|p| p != "openai")
        {
            bail!("provider must be openai");
        }
        if !matches!(self.transport.as_str(), "auto" | "http") {
            bail!("transport must be auto or http");
        }
        if self.behaves_as.trim().is_empty() {
            bail!("behaves_as must name a Claude model");
        }
        check_url("upstream_base_url", &self.upstream_base_url)?;
        for (id, provider) in &self.providers {
            if id.is_empty()
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            {
                bail!("provider ID {id:?} may contain only letters, digits, '-', '_' and '.'");
            }
            if !matches!(
                provider.protocol.as_str(),
                "" | "anthropic" | "openai-chat" | "openai-responses"
            ) {
                bail!("providers.{id}.protocol must be anthropic, openai-chat or openai-responses");
            }
            if !provider.base_url.is_empty() {
                check_url(&format!("providers.{id}.base_url"), &provider.base_url)?;
            }
        }
        Ok(())
    }
}

/// Read a setting by dotted path, such as `aliases.haiku` or `providers.zai.models`.
/// Split a dotted setting path; `fallbacks.<model ID>` keeps the model ID whole.
fn key_parts(key: &str) -> Vec<&str> {
    match key.strip_prefix("fallbacks.") {
        Some(model) => vec!["fallbacks", model],
        None => key.split('.').filter(|p| !p.is_empty()).collect(),
    }
}

pub fn cli_get(key: &str) -> Result<()> {
    let value = serde_json::to_value(load()?)?;
    let found = key_parts(key)
        .into_iter()
        .try_fold(&value, |v, part| v.get(part));
    match found {
        Some(serde_json::Value::String(s)) => println!("{s}"),
        Some(other) => println!("{}", serde_json::to_string_pretty(other)?),
        None => bail!("no setting {key:?}"),
    }
    Ok(())
}

/// Parse a command-line value for a setting: booleans and numbers where the schema expects
/// them, comma-separated lists for list settings, strings otherwise.
fn parse_value(key: &str, value: &str) -> serde_json::Value {
    use serde_json::Value;
    let leaf = key.rsplit('.').next().unwrap_or(key);
    if leaf == "models" || key.starts_with("fallbacks.") {
        return Value::Array(
            value
                .split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|v| Value::String(v.into()))
                .collect(),
        );
    }
    match value {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => value
            .parse::<u64>()
            .ok()
            .filter(|_| leaf == "context_tokens")
            .map(Value::from)
            .unwrap_or_else(|| Value::String(value.into())),
    }
}

/// Set (or with `None`, remove) a setting, validating the result before writing.
pub fn cli_set(key: &str, value: Option<&str>) -> Result<()> {
    use serde_json::{Map, Value};
    let path = state_dir()?.join("config.json");
    let mut root = match crate::store::read_private(&path)? {
        Some(bytes) => serde_json::from_slice::<Value>(&bytes).context("parsing config.json")?,
        None => Value::Object(Map::new()),
    };
    let parts = key_parts(key);
    let Some((last, parents)) = parts.split_last() else {
        bail!("empty setting name");
    };
    let mut node = &mut root;
    for part in parents {
        let map = node
            .as_object_mut()
            .context("setting path crosses a non-object value")?;
        node = map
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let map = node
        .as_object_mut()
        .context("setting path crosses a non-object value")?;
    match value {
        Some(value) => {
            map.insert(last.to_string(), parse_value(key, value));
        }
        None => {
            map.remove(*last);
        }
    }
    let config: Config = serde_json::from_value(root.clone())
        .with_context(|| format!("{key:?} is not a valid setting"))?;
    config.validate()?;
    crate::store::write_private(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&root)?).as_bytes(),
    )?;
    match value {
        Some(value) => println!("{key} = {value}"),
        None => println!("{key} reset to default"),
    }
    Ok(())
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
        assert!(!config.relay);
        assert!(!config.teams);
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
            PathBuf::from("/home/user/.byom")
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
    fn cli_values_follow_the_schema() {
        assert_eq!(parse_value("relay", "false"), serde_json::json!(false));
        assert_eq!(
            parse_value("context_tokens", "128000"),
            serde_json::json!(128000)
        );
        assert_eq!(parse_value("model", "123"), serde_json::json!("123"));
        assert_eq!(
            parse_value("providers.zai.models", "glm-5, glm-4.7"),
            serde_json::json!(["glm-5", "glm-4.7"])
        );
        assert_eq!(
            parse_value("fallbacks.openai/gpt-6-astra", "openai/gpt-5.6-sol"),
            serde_json::json!(["openai/gpt-5.6-sol"])
        );
    }

    #[test]
    fn io_errors_do_not_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_from_path(dir.path()).is_err());
    }
}
