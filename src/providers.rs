//! Providers: where a model ID is served, over which protocol, with which credential.
//!
//! Model IDs are `provider/model` (`openai/gpt-5.6-sol`, `kimi/kimi-k3`). Claude models keep
//! their native IDs (`claude-opus-5-5`) and go to the `anthropic` provider. A bare non-Claude
//! ID from a 0.1.0 config is treated as an `openai` model.
use crate::config::{Config, ProviderConfig};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// Anthropic Messages, forwarded with a base URL and credential swap.
    Anthropic,
    /// OpenAI Chat Completions, translated.
    OpenAiChat,
    /// OpenAI Responses, translated.
    OpenAiResponses,
}

impl Protocol {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "anthropic" => Some(Self::Anthropic),
            "openai-chat" => Some(Self::OpenAiChat),
            "openai-responses" => Some(Self::OpenAiResponses),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    /// Claude Code's own credential, relayed as sent.
    ClaudeCode,
    /// Sign in with ChatGPT session in auth.json.
    ChatGpt,
    /// An API key from config or auth.json.
    ApiKey,
    /// No credential (local servers).
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub auth: Auth,
    /// Where to get a key or sign up, shown by `login`.
    pub signup: String,
    /// Terms or usage notes shown by `login` and `models`.
    pub note: String,
    /// Models to offer when the provider has no model list endpoint.
    pub models: Vec<String>,
    pub builtin: bool,
}

struct Builtin {
    id: &'static str,
    name: &'static str,
    protocol: Protocol,
    base_url: &'static str,
    auth: Auth,
    signup: &'static str,
    note: &'static str,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        id: "anthropic",
        name: "Anthropic (Claude)",
        protocol: Protocol::Anthropic,
        base_url: "https://api.anthropic.com",
        auth: Auth::ClaudeCode,
        signup: "https://claude.com/claude-code",
        note: "Uses Claude Code's own sign-in; requests are relayed unmodified.",
    },
    Builtin {
        id: "openai",
        name: "OpenAI (ChatGPT plan)",
        protocol: Protocol::OpenAiResponses,
        base_url: "https://api.openai.com/v1",
        auth: Auth::ChatGpt,
        signup: "https://chatgpt.com",
        note: "Sign in with ChatGPT; usage counts against your plan and byoclaude's app limit.",
    },
];

pub const CLAUDE_PROVIDER: &str = "anthropic";

fn from_builtin(b: &Builtin) -> Provider {
    Provider {
        id: b.id.into(),
        name: b.name.into(),
        protocol: b.protocol,
        base_url: b.base_url.into(),
        auth: b.auth,
        signup: b.signup.into(),
        note: b.note.into(),
        models: Vec::new(),
        builtin: true,
    }
}

fn apply(mut provider: Provider, custom: &ProviderConfig) -> Provider {
    if !custom.name.is_empty() {
        provider.name = custom.name.clone();
    }
    if let Some(protocol) = Protocol::parse(&custom.protocol) {
        provider.protocol = protocol;
    }
    if !custom.base_url.is_empty() {
        provider.base_url = custom.base_url.clone();
    }
    if !custom.models.is_empty() {
        provider.models = custom.models.clone();
    }
    if !custom.api_key.is_empty() && provider.auth != Auth::ChatGpt {
        provider.auth = Auth::ApiKey;
    }
    provider
}

/// Every enabled provider: built-ins with config overrides, then user-defined ones.
pub fn all(config: &Config) -> Vec<Provider> {
    let mut providers: Vec<Provider> = BUILTINS
        .iter()
        .map(|b| {
            let provider = from_builtin(b);
            match config.providers.get(b.id) {
                Some(custom) => apply(provider, custom),
                None => provider,
            }
        })
        .collect();
    // 0.1.0 kept the OpenAI base URL at the top level; tests point it at a mock.
    if let Some(openai) = providers.iter_mut().find(|p| p.id == "openai")
        && !config
            .providers
            .get("openai")
            .is_some_and(|c| !c.base_url.is_empty())
    {
        openai.base_url = config.upstream_base_url.clone();
    }
    for (id, custom) in &config.providers {
        if BUILTINS.iter().any(|b| b.id == id) {
            continue;
        }
        let base = Provider {
            id: id.clone(),
            name: id.clone(),
            protocol: Protocol::Anthropic,
            base_url: String::new(),
            auth: Auth::None,
            signup: String::new(),
            note: String::new(),
            models: Vec::new(),
            builtin: false,
        };
        providers.push(apply(base, custom));
    }
    providers.retain(|p| !config.providers.get(&p.id).is_some_and(|c| c.disabled));
    providers
}

pub fn find(config: &Config, id: &str) -> Option<Provider> {
    all(config).into_iter().find(|p| p.id == id)
}

pub fn is_claude(model: &str) -> bool {
    !model.contains('/') && model.starts_with("claude")
}

/// Split a model ID into its provider ID and the model name the provider expects.
pub fn split(model: &str) -> (&str, &str) {
    match model.split_once('/') {
        Some((provider, name)) if !provider.is_empty() && !name.is_empty() => (provider, name),
        _ if is_claude(model) => (CLAUDE_PROVIDER, model),
        _ => ("openai", model),
    }
}

/// Canonical ID for a model name: bare 0.1.0 OpenAI IDs gain the `openai/` prefix.
pub fn canonical(model: &str) -> String {
    if model.is_empty() || model.contains('/') || is_claude(model) {
        model.to_owned()
    } else {
        format!("openai/{model}")
    }
}

/// Resolve a model ID to its provider and upstream model name.
pub fn route(config: &Config, model: &str) -> Option<(Provider, String)> {
    let (provider, name) = split(model);
    find(config, provider).map(|p| (p, name.to_owned()))
}

/// Resolve a configured key reference: `$ENV_VAR`, `!command`, or a literal key.
pub fn resolve_key(reference: &str) -> anyhow::Result<String> {
    use anyhow::{Context, bail};
    let key = if let Some(var) = reference.strip_prefix('$') {
        std::env::var(var).with_context(|| format!("environment variable {var} is not set"))?
    } else if let Some(command) = reference.strip_prefix('!') {
        let output = std::process::Command::new("sh")
            .args(["-c", command])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .context("running API key command")?;
        if !output.status.success() {
            bail!("API key command exited with {}", output.status);
        }
        String::from_utf8(output.stdout).context("API key command printed invalid UTF-8")?
    } else {
        reference.to_owned()
    };
    let key = key.trim().to_owned();
    if key.is_empty() || key.chars().any(char::is_control) {
        bail!("API key is empty or contains control characters");
    }
    Ok(key)
}

/// The API key for a provider: config reference first, then auth.json.
pub fn api_key(config: &Config, provider: &Provider) -> anyhow::Result<Option<String>> {
    if let Some(reference) = config
        .providers
        .get(&provider.id)
        .map(|c| c.api_key.as_str())
        && !reference.is_empty()
    {
        return resolve_key(reference).map(Some);
    }
    if matches!(provider.auth, Auth::ChatGpt | Auth::None) {
        return Ok(None);
    }
    Ok(crate::store::auth::get(&provider.id)?.and_then(|v| v["key"].as_str().map(str::to_owned)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_references_resolve() {
        assert_eq!(resolve_key("sk-1").unwrap(), "sk-1");
        assert_eq!(resolve_key("!printf ' sk-2\\n'").unwrap(), "sk-2");
        assert!(resolve_key("$BYOCLAUDE_TEST_UNSET_VAR").is_err());
        assert!(resolve_key("!exit 3").is_err());
    }

    #[test]
    fn routes_by_model_id() {
        let config = Config::default();
        let (p, m) = route(&config, "openai/gpt-5.6-sol").unwrap();
        assert_eq!((p.id.as_str(), m.as_str()), ("openai", "gpt-5.6-sol"));
        let (p, m) = route(&config, "claude-opus-5-5").unwrap();
        assert_eq!(
            (p.protocol, m.as_str()),
            (Protocol::Anthropic, "claude-opus-5-5")
        );
        let (p, _) = route(&config, "gpt-5.6-sol").unwrap();
        assert_eq!(p.id, "openai");
        assert!(route(&config, "nope/model").is_none());
        assert_eq!(canonical("gpt-5.5"), "openai/gpt-5.5");
        assert_eq!(canonical("claude-haiku-4-5"), "claude-haiku-4-5");
    }

    #[test]
    fn config_overrides_and_custom_providers() {
        let mut config = Config::default();
        config.providers.insert(
            "anthropic".into(),
            ProviderConfig {
                base_url: "http://127.0.0.1:9/".into(),
                ..Default::default()
            },
        );
        config.providers.insert(
            "local".into(),
            ProviderConfig {
                protocol: "openai-chat".into(),
                base_url: "http://127.0.0.1:8080/v1".into(),
                models: vec!["qwen".into()],
                ..Default::default()
            },
        );
        config.providers.insert(
            "openai".into(),
            ProviderConfig {
                disabled: true,
                ..Default::default()
            },
        );
        assert_eq!(
            find(&config, "anthropic").unwrap().base_url,
            "http://127.0.0.1:9/"
        );
        let local = find(&config, "local").unwrap();
        assert_eq!(
            (local.protocol, local.auth, local.models.len()),
            (Protocol::OpenAiChat, Auth::None, 1)
        );
        assert!(find(&config, "openai").is_none());
    }
}
