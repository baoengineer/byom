use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};

const DEFAULT_PORT: u16 = 47391;

fn port() -> u16 {
    std::env::var("BYOCLAUDE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}
const START_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(PROBE_TIMEOUT)
        .build()
        .context("creating local bridge health client")
}

async fn key() -> Result<Option<String>> {
    read_key(&crate::config::state_dir()?.join("bridge.key"))
}

fn read_key(path: &std::path::Path) -> Result<Option<String>> {
    use std::io::Read;
    let entry = match std::fs::symlink_metadata(path) {
        Ok(entry) => entry,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("checking local bridge key"),
    };
    if !entry.is_file() || entry.file_type().is_symlink() {
        bail!("local bridge key must be a regular file, not a symlink");
    }
    let file = std::fs::File::open(path).context("opening local bridge key")?;
    let metadata = file
        .metadata()
        .context("checking opened local bridge key")?;
    if !metadata.is_file() {
        bail!("local bridge key must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o777 != 0o600
            || metadata.ino() != entry.ino()
            || metadata.dev() != entry.dev()
        {
            bail!("local bridge key must have mode 0600 and must not be replaced while opening");
        }
    }
    let mut key = String::new();
    file.take(65)
        .read_to_string(&mut key)
        .context("reading local bridge key")?;
    if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("local bridge key must be exactly 64 hexadecimal characters");
    }
    Ok(Some(key))
}

const UNUSED_PROVIDER_AUTH: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "ANTHROPIC_API_KEY_FILE_DESCRIPTOR",
    "OPENAI_API_KEY",
    "OPENAI_AUTH_TOKEN",
    "OPENROUTER_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    "AWS_BEARER_TOKEN_BEDROCK",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_API_KEY",
    "CLOUD_ML_API_KEY",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "ANTHROPIC_FOUNDRY_BASE_URL",
];
const ALTERNATE_ROUTES: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

fn sanitize_claude(command: &mut std::process::Command) {
    for name in UNUSED_PROVIDER_AUTH.iter().chain(ALTERNATE_ROUTES) {
        command.env_remove(name);
    }
}

/// With the relay, Claude Code keeps its own Anthropic credentials.
fn sanitize_relay(command: &mut std::process::Command) {
    for name in UNUSED_PROVIDER_AUTH.iter().chain(ALTERNATE_ROUTES) {
        if !name.starts_with("ANTHROPIC_") && !name.starts_with("CLAUDE_CODE_OAUTH") {
            command.env_remove(name);
        }
    }
}

fn sanitize_bridge(command: &mut Command) {
    for name in UNUSED_PROVIDER_AUTH.iter().chain(ALTERNATE_ROUTES).chain(&[
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ]) {
        command.env_remove(name);
    }
}

/// Version of the authenticated bridge on the port, if one is ready.
async fn ready(client: &reqwest::Client, key: &str) -> Option<String> {
    // This detects accidental port conflicts, not a malicious local impersonator.
    let url = format!("http://127.0.0.1:{}/health", port());
    match client.get(&url).send().await {
        Ok(response) if matches!(response.status().as_u16(), 401 | 403) => {}
        _ => return None,
    }
    // Do not follow redirects or use proxies: the bearer stays on loopback.
    match client
        .get(format!("http://127.0.0.1:{}/health", port()))
        .bearer_auth(key)
        .send()
        .await
    {
        Ok(response) if response.status() == reqwest::StatusCode::OK => {
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            Some(body["version"].as_str().unwrap_or("unknown").to_owned())
        }
        _ => None,
    }
}

/// Version of the running bridge, if one answers with this install's key.
pub async fn bridge_version() -> Option<String> {
    let key = key().await.ok()??;
    ready(&client().ok()?, &key).await
}

async fn request_shutdown(client: &reqwest::Client, key: &str) -> bool {
    client
        .post(format!("http://127.0.0.1:{}/shutdown", port()))
        .bearer_auth(key)
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

async fn port_free() -> bool {
    !matches!(
        tokio::time::timeout(
            PROBE_TIMEOUT,
            tokio::net::TcpStream::connect(("127.0.0.1", port()))
        )
        .await,
        Ok(Ok(_))
    )
}

async fn ensure_bridge(client: &reqwest::Client) -> Result<String> {
    if let Some(key) = key().await?
        && let Some(version) = ready(client, &key).await
    {
        if version == env!("CARGO_PKG_VERSION") {
            return Ok(key);
        }
        // Replace a bridge left running by another byoclaude version.
        if !request_shutdown(client, &key).await {
            bail!(
                "a byoclaude {version} bridge is running on port {} and cannot be stopped automatically; stop that process and retry",
                port()
            );
        }
        let deadline = Instant::now() + START_TIMEOUT;
        while !port_free().await {
            if Instant::now() >= deadline {
                bail!("the previous bridge did not stop within 5 seconds");
            }
            sleep(Duration::from_millis(100)).await;
        }
    }
    // An occupied port without authenticated readiness must not be used.
    if matches!(
        tokio::time::timeout(
            PROBE_TIMEOUT,
            tokio::net::TcpStream::connect(("127.0.0.1", port()))
        )
        .await,
        Ok(Ok(_))
    ) {
        bail!(
            "port {} is occupied but authenticated bridge readiness failed; refusing to launch Claude",
            port()
        );
    }
    let executable = std::env::current_exe().context("locating current launcher executable")?;
    let mut command = Command::new(executable);
    sanitize_bridge(&mut command);
    let mut child = command
        .args(["bridge", "--port", &port().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(bridge_stderr())
        .kill_on_drop(false)
        .spawn()
        .context("starting local bridge")?;
    wait_for_bridge(client, &mut child).await
}

/// Bridge diagnostics go to a file, not the terminal Claude Code draws on.
fn bridge_stderr() -> Stdio {
    crate::store::logs_dir()
        .ok()
        .and_then(|dir| {
            crate::store::private_dir(&dir).ok()?;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("bridge.err"))
                .ok()
        })
        .map(Stdio::from)
        .unwrap_or_else(Stdio::null)
}

async fn wait_for_bridge(client: &reqwest::Client, child: &mut Child) -> Result<String> {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut exit = None;
    loop {
        // Another concurrent launcher may win the bind race. Accept only its
        // authenticated health response, not the spawned child's exit status.
        if let Some(key) = key().await?
            && ready(client, &key).await.is_some()
        {
            return Ok(key);
        }
        if exit.is_none() {
            exit = child.try_wait().context("checking bridge startup")?;
        }
        if Instant::now() >= deadline {
            if let Some(status) = exit {
                bail!("bridge exited with {status}; authenticated readiness was not reached");
            }
            bail!(
                "bridge did not reach authenticated readiness within 5 seconds; it may still be starting (use status)"
            );
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// What a session runs on: model slots, context window and the model picker.
///
/// With the relay active, unset slots keep Claude Code's own Claude defaults; without it,
/// every slot needs a non-Claude model.
pub struct Plan {
    pub relay: bool,
    /// Main model (`ANTHROPIC_MODEL`); `None` keeps Claude Code's default.
    pub model: Option<String>,
    /// Background model for titles and summaries (the haiku slot).
    pub background: Option<String>,
    pub opus: Option<String>,
    pub sonnet: Option<String>,
    pub subagent: Option<String>,
    /// Context window to compact against; `None` keeps Claude Code's value.
    pub context_tokens: Option<u64>,
    pub settings: serde_json::Value,
    /// Non-Claude models offered in /model and as subagents.
    pub rows: Vec<Row>,
}

/// A model row offered in `/model` and to subagents.
pub struct Row {
    pub id: String,
    pub label: String,
    pub description: String,
}

/// Every non-Claude model byoclaude can route: the cached roster, the ChatGPT catalog and
/// models named in config.
pub fn rows(
    config: &crate::config::Config,
    models: &[crate::catalog::Model],
    roster: &[crate::catalog::Entry],
) -> Vec<Row> {
    let providers = crate::providers::all(config);
    let provider_name = |id: &str| {
        providers
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| id.to_owned())
    };
    let mut rows: Vec<Row> = Vec::new();
    for entry in roster.iter().filter(|e| {
        e.listed && e.provider != crate::providers::CLAUDE_PROVIDER && e.provider != "openai"
    }) {
        if !providers.iter().any(|p| p.id == entry.provider) {
            continue;
        }
        let (_, model) = crate::providers::split(&entry.id);
        rows.push(Row {
            id: entry.id.clone(),
            label: if entry.name.is_empty() {
                model.to_owned()
            } else {
                entry.name.clone()
            },
            description: provider_name(&entry.provider),
        });
    }
    if providers.iter().any(|p| p.id == "openai") {
        rows.extend(models.iter().filter(|m| m.listed).map(|m| {
            Row {
                id: crate::providers::canonical(&m.slug),
                label: if m.display_name.is_empty() {
                    m.slug.clone()
                } else {
                    m.display_name.clone()
                },
                description: format!("{} (ChatGPT plan)", m.description)
                    .trim()
                    .to_owned(),
            }
        }));
    }
    for provider in &providers {
        // Configured models need a usable provider to be offered.
        let usable = match provider.auth {
            crate::providers::Auth::ApiKey => crate::providers::api_key(config, provider)
                .ok()
                .flatten()
                .is_some(),
            crate::providers::Auth::ChatGpt => crate::store::auth::get(&provider.id)
                .ok()
                .flatten()
                .is_some(),
            _ => true,
        };
        if !usable {
            continue;
        }
        for model in &provider.models {
            let id = format!("{}/{model}", provider.id);
            if !rows.iter().any(|r| r.id == id) {
                rows.push(Row {
                    id,
                    label: model.clone(),
                    description: provider.name.clone(),
                });
            }
        }
    }
    rows
}

pub fn plan(
    config: &crate::config::Config,
    models: &[crate::catalog::Model],
    roster: &[crate::catalog::Entry],
    requested: Option<String>,
    relay: bool,
) -> Result<Plan> {
    use crate::providers::{canonical, is_claude};
    let mut rows = rows(config, models, roster);
    let pick = |value: &str| (!value.trim().is_empty()).then(|| canonical(value.trim()));
    let first = rows.first().map(|r| r.id.clone());
    let model = requested
        .as_deref()
        .and_then(pick)
        .or_else(|| pick(&config.model));
    let model = match model {
        Some(model) => Some(model),
        None if relay => None,
        None => Some(first.clone().context(
            "no models available. Sign in with `byoclaude login`, or sign in to Claude Code to use Claude models",
        )?),
    };
    if !relay && model.as_deref().is_some_and(is_claude) {
        bail!(
            "Claude models need Claude Code signed in to Claude (`claude auth login`) and \"relay\" enabled"
        );
    }
    // Without the relay, Claude's own slots are unusable, so they follow byoclaude's choices.
    let small = rows
        .iter()
        .find(|r| r.id.contains("luna") || r.id.contains("mini"))
        .map(|r| r.id.clone());
    let background = pick(&config.background)
        .or_else(|| pick(&config.aliases.haiku))
        .or_else(|| {
            (!relay)
                .then(|| small.clone().or_else(|| model.clone()))
                .flatten()
        });
    let follow = |alias: &str| pick(alias).or_else(|| (!relay).then(|| model.clone()).flatten());
    let opus = follow(&config.aliases.opus);
    let sonnet = follow(&config.aliases.sonnet);
    let subagent = pick(&config.subagent);
    let context_tokens = if config.context_tokens > 0 {
        Some(config.context_tokens)
    } else {
        model.as_deref().filter(|m| !is_claude(m)).map(|m| {
            let (provider, name) = crate::providers::split(m);
            let from_roster = roster.iter().find(|e| e.id == m).map(|e| e.context_window);
            let from_chatgpt = (provider == "openai")
                .then(|| crate::catalog::find(models, name).map(|c| c.context_window))
                .flatten();
            from_roster
                .or(from_chatgpt)
                .filter(|w| *w > 0)
                .unwrap_or(200_000)
        })
    };
    // Models named in config but missing from any catalog still need a row to be accepted.
    for id in [&model, &background, &opus, &sonnet, &subagent]
        .into_iter()
        .flatten()
    {
        if !is_claude(id) && !rows.iter().any(|r| &r.id == id) {
            rows.push(Row {
                id: id.clone(),
                label: id.clone(),
                description: String::new(),
            });
        }
    }
    let options: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "model": r.id,
                "label": r.label,
                "description": r.description,
                "behavesAs": config.behaves_as,
            })
        })
        .collect();
    let settings = serde_json::json!({
        "modelPicker": {"replaceBuiltInOptions": !relay, "options": options}
    });
    Ok(Plan {
        relay,
        model,
        background,
        opus,
        sonnet,
        subagent,
        context_tokens,
        settings,
        rows,
    })
}

/// Whether Claude Code is signed in (to a Claude plan, an Anthropic key or a token), as
/// reported by `claude auth status --json` under the launch environment.
pub fn claude_signed_in() -> bool {
    let output = std::process::Command::new("claude")
        .args(["auth", "status", "--json"])
        .env_remove("ANTHROPIC_BASE_URL")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return false;
    };
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    status["loggedIn"] == true
        && status["authMethod"].as_str().is_some_and(|m| m != "none")
        && status["apiProvider"]
            .as_str()
            .is_none_or(|p| p == "firstParty")
}

const RELAY_NOTE: &str = "byoclaude: Claude models in this session reach Anthropic through byoclaude's local relay, using Claude Code's own sign-in. Request bodies and Claude Code's headers pass through unchanged, but Anthropic has not explicitly approved relaying subscription traffic. To keep Claude Code talking to Anthropic directly, run: byoclaude config set relay false";

/// Print the relay note once per state directory.
pub fn relay_note() {
    let Ok(marker) = crate::store::home().map(|h| h.join(".relay-note-shown")) else {
        return;
    };
    if marker.exists() {
        return;
    }
    eprintln!("{RELAY_NOTE}\n");
    let _ = crate::store::write_private(&marker, b"");
}

/// Start the bridge if needed and return its key.
pub async fn bridge_key() -> Result<String> {
    ensure_bridge(&client()?).await
}

/// The `claude` command for a plan: sanitized environment, the bridge's base URL and key,
/// model slots, and `--settings` with the model picker rows. Arguments added later follow.
pub fn claude_command(
    plan: &Plan,
    key: &str,
    plugin: Option<&std::path::Path>,
) -> std::process::Command {
    let mut command = std::process::Command::new("claude");
    if plan.relay {
        sanitize_relay(&mut command);
    } else {
        sanitize_claude(&mut command);
    }
    let bridge_header = format!("{}: {key}", crate::relay::BRIDGE_KEY_HEADER);
    let headers = match std::env::var("ANTHROPIC_CUSTOM_HEADERS") {
        Ok(existing) if !existing.trim().is_empty() => format!("{existing}\n{bridge_header}"),
        _ => bridge_header,
    };
    command.arg("--settings").arg(plan.settings.to_string());
    if let Some(plugin) = plugin {
        command.arg("--plugin-dir").arg(plugin);
    }
    command
        .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{}", port()))
        .env("ANTHROPIC_CUSTOM_HEADERS", headers)
        // Tool search defers tool definitions that non-Claude models never receive.
        .env("ENABLE_TOOL_SEARCH", "false");
    if !plan.relay {
        command
            .env("ANTHROPIC_AUTH_TOKEN", key)
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            // claude.ai connectors need claude.ai auth, which the bridge key replaces; turning
            // them off also removes Claude Code's warning about it.
            .env("ENABLE_CLAUDEAI_MCP_SERVERS", "false");
    }
    let slots = [
        ("ANTHROPIC_MODEL", &plan.model),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", &plan.opus),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", &plan.sonnet),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", &plan.background),
        ("ANTHROPIC_SMALL_FAST_MODEL", &plan.background),
        ("CLAUDE_CODE_SUBAGENT_MODEL", &plan.subagent),
    ];
    for (name, value) in slots {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    if let Some(tokens) = plan.context_tokens {
        command
            .env("CLAUDE_CODE_MAX_CONTEXT_TOKENS", tokens.to_string())
            .env("CLAUDE_CODE_AUTO_COMPACT_WINDOW", tokens.to_string());
    }
    command
}

/// Launch Claude Code against the local bridge.
pub async fn run(model: Option<String>, args: Vec<String>) -> Result<()> {
    let config = crate::config::load()?;
    let relay = config.relay && claude_signed_in();
    // Any usable provider is enough; the plan reports when there is none.
    let models = crate::catalog::load().await.unwrap_or_default();
    if !crate::catalog::roster_exists() {
        crate::catalog::refresh(&config).await;
    }
    let plan = plan(
        &config,
        &models,
        &crate::catalog::roster_cached(),
        model,
        relay,
    )?;
    let key = ensure_bridge(&client()?).await?;
    if relay {
        relay_note();
    }
    let plugin = crate::skill::prepare(&plan.rows)?;
    let mut command = claude_command(&plan, &key, Some(&plugin));
    command.args(args);
    // The session mod starts panels with this binary and reads the gate setting.
    if let Ok(exe) = std::env::current_exe() {
        command.env("BYOCLAUDE_BIN", exe);
    }
    if config.panel.gate {
        command.env("BYOCLAUDE_PANEL_GATE", "1");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("executing installed claude command")
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .context("running installed claude command")?;
        if !status.success() {
            bail!("claude exited with {status}");
        }
        Ok(())
    }
}

pub async fn status() -> Result<()> {
    let client = client()?;
    if let Some(key) = key().await?
        && let Some(version) = ready(&client, &key).await
    {
        println!(
            "Bridge {version} ready at http://127.0.0.1:{}; request log: {}",
            port(),
            crate::store::log_path()?.display()
        );
        return Ok(());
    }
    bail!(
        "Bridge is not running on port {}. byoclaude run starts it.",
        port()
    );
}

pub async fn stop() -> Result<()> {
    let client = client()?;
    let Some(key) = key().await? else {
        println!("Bridge is not running");
        return Ok(());
    };
    if ready(&client, &key).await.is_none() {
        println!("Bridge is not running");
        return Ok(());
    }
    if !request_shutdown(&client, &key).await {
        bail!("the running bridge does not accept shutdown requests; stop its process directly");
    }
    println!("Bridge stopping");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(slug: &str, listed: bool) -> crate::catalog::Model {
        crate::catalog::Model {
            slug: slug.into(),
            display_name: slug.to_uppercase(),
            description: String::new(),
            context_window: 272_000,
            effort_levels: vec!["low".into()],
            default_effort: None,
            listed,
        }
    }

    #[test]
    fn plan_without_relay_fills_every_slot() {
        let models = [
            model("big", true),
            model("hidden", false),
            model("gpt-luna", true),
        ];
        let plan = plan(&crate::config::Config::default(), &models, &[], None, false).unwrap();
        assert_eq!(plan.model.as_deref(), Some("openai/big"));
        assert_eq!(plan.background.as_deref(), Some("openai/gpt-luna"));
        assert_eq!(plan.opus.as_deref(), Some("openai/big"));
        assert_eq!(plan.context_tokens, Some(272_000));
        let rows = plan.settings["modelPicker"]["options"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["model"], "openai/big");
        assert_eq!(rows[0]["behavesAs"], "claude-opus-5-5");
        assert_eq!(plan.settings["modelPicker"]["replaceBuiltInOptions"], true);
        assert!(super::plan(&crate::config::Config::default(), &[], &[], None, false).is_err());
        let claude = crate::config::Config {
            model: "claude-opus-5-5".into(),
            ..Default::default()
        };
        assert!(super::plan(&claude, &models, &[], None, false).is_err());
    }

    #[test]
    fn plan_with_relay_keeps_claude_defaults() {
        let models = [model("big", true), model("gpt-luna", true)];
        let plan = plan(&crate::config::Config::default(), &models, &[], None, true).unwrap();
        assert_eq!(plan.model, None);
        assert_eq!(plan.background, None);
        assert_eq!(plan.opus, None);
        assert_eq!(plan.context_tokens, None);
        assert_eq!(plan.settings["modelPicker"]["replaceBuiltInOptions"], false);
        // No ChatGPT sign-in: Claude alone still works.
        assert!(super::plan(&crate::config::Config::default(), &[], &[], None, true).is_ok());
        // A GPT main model gets its window; Claude aliases stay native.
        let plan = super::plan(
            &crate::config::Config::default(),
            &models,
            &[],
            Some("gpt-luna".into()),
            true,
        )
        .unwrap();
        assert_eq!(plan.model.as_deref(), Some("openai/gpt-luna"));
        assert_eq!(plan.context_tokens, Some(272_000));
        assert_eq!(plan.sonnet, None);
    }

    #[test]
    fn plan_honors_configured_roles_and_unknown_models() {
        let mut config = crate::config::Config {
            model: "configured".into(),
            background: "openai/small".into(),
            subagent: "kimi/kimi-k3".into(),
            context_tokens: 1000,
            ..Default::default()
        };
        config.aliases.opus = "claude-opus-5-5".into();
        let plan = plan(
            &config,
            &[model("big", true)],
            &[],
            Some("requested".into()),
            true,
        )
        .unwrap();
        assert_eq!(plan.model.as_deref(), Some("openai/requested"));
        assert_eq!(plan.background.as_deref(), Some("openai/small"));
        assert_eq!(plan.subagent.as_deref(), Some("kimi/kimi-k3"));
        assert_eq!(plan.opus.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(plan.context_tokens, Some(1000));
        // Models missing from any catalog still get a picker row so Claude Code accepts them.
        let rows = plan.settings["modelPicker"]["options"].as_array().unwrap();
        for id in ["openai/requested", "openai/small", "kimi/kimi-k3"] {
            assert!(rows.iter().any(|r| r["model"] == id), "{id}");
        }
        assert!(!rows.iter().any(|r| r["model"] == "claude-opus-5-5"));
    }

    #[test]
    fn child_environment_is_sanitized_without_clearing_system_settings() {
        let mut claude = std::process::Command::new("unused");
        let mut bridge = Command::new("unused");
        for name in UNUSED_PROVIDER_AUTH.iter().chain(ALTERNATE_ROUTES) {
            claude.env(name, "synthetic-unused");
            bridge.env(name, "synthetic-unused");
        }
        for name in ["PATH", "EDITOR", "VISUAL"] {
            claude.env(name, "synthetic-preserved");
            bridge.env(name, "synthetic-preserved");
        }
        bridge.env("ANTHROPIC_BASE_URL", "synthetic-unused");
        sanitize_claude(&mut claude);
        sanitize_bridge(&mut bridge);
        for command in [&claude, bridge.as_std()] {
            let env: std::collections::HashMap<_, _> = command.get_envs().collect();
            for name in UNUSED_PROVIDER_AUTH.iter().chain(ALTERNATE_ROUTES) {
                assert_eq!(env[std::ffi::OsStr::new(name)], None, "{name}");
            }
            for name in ["PATH", "EDITOR", "VISUAL"] {
                assert_eq!(
                    env[std::ffi::OsStr::new(name)],
                    Some(std::ffi::OsStr::new("synthetic-preserved"))
                );
            }
        }
        assert_eq!(
            bridge
                .as_std()
                .get_envs()
                .find(|(name, _)| *name == "ANTHROPIC_BASE_URL")
                .unwrap()
                .1,
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_reader_rejects_unsafe_and_malformed_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.key");
        assert!(read_key(&path).unwrap().is_none());
        std::fs::create_dir(&path).unwrap();
        assert!(read_key(&path).is_err());
        std::fs::remove_dir(&path).unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "a".repeat(64)).unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_key(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "a".repeat(64)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_key(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_key(&path).unwrap(), Some("a".repeat(64)));
        for value in [
            String::new(),
            "a".repeat(63),
            "a".repeat(65),
            "g".repeat(64),
            format!("{}\n", "a".repeat(64)),
        ] {
            std::fs::write(&path, value).unwrap();
            assert!(read_key(&path).is_err());
        }
    }

    #[test]
    fn health_client_is_constructible() {
        client().unwrap();
    }
}
