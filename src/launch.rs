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
    crate::config::state_dir()
        .ok()
        .and_then(|dir| {
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

/// Picker rows and environment for running Claude Code on ChatGPT-plan models.
pub struct Plan {
    pub model: String,
    pub small_model: String,
    pub context_tokens: u64,
    pub settings: serde_json::Value,
}

pub fn plan(
    config: &crate::config::Config,
    models: &[crate::catalog::Model],
    requested: Option<String>,
) -> Result<Plan> {
    let listed: Vec<&crate::catalog::Model> = models.iter().filter(|m| m.listed).collect();
    let model = requested
        .filter(|m| !m.trim().is_empty())
        .or_else(|| (!config.model.is_empty()).then(|| config.model.clone()))
        .or_else(|| listed.first().map(|m| m.slug.clone()))
        .context("no models available; run byoclaude models to check your ChatGPT plan")?;
    // Background work (titles, summaries) goes to the smallest listed model.
    let small_model = (!config.small_model.is_empty())
        .then(|| config.small_model.clone())
        .or_else(|| {
            listed
                .iter()
                .find(|m| m.slug.contains("luna") || m.slug.contains("mini"))
                .map(|m| m.slug.clone())
        })
        .unwrap_or_else(|| model.clone());
    let context_tokens = if config.context_tokens > 0 {
        config.context_tokens
    } else {
        crate::catalog::find(models, &model)
            .map(|m| m.context_window)
            .filter(|w| *w > 0)
            .unwrap_or(200_000)
    };
    let mut options: Vec<serde_json::Value> = listed
        .iter()
        .map(|m| {
            serde_json::json!({
                "model": m.slug,
                "label": if m.display_name.is_empty() { &m.slug } else { &m.display_name },
                "description": format!("{} (ChatGPT plan)", m.description),
                "behavesAs": config.behaves_as,
            })
        })
        .collect();
    for slug in [&model, &small_model] {
        if !options.iter().any(|o| o["model"] == slug.as_str()) {
            options.push(
                serde_json::json!({"model": slug, "label": slug, "behavesAs": config.behaves_as}),
            );
        }
    }
    let settings =
        serde_json::json!({"modelPicker": {"replaceBuiltInOptions": true, "options": options}});
    Ok(Plan {
        model,
        small_model,
        context_tokens,
        settings,
    })
}

/// Launch Claude Code against the local bridge.
pub async fn run(model: Option<String>, args: Vec<String>) -> Result<()> {
    let config = crate::config::load()?;
    // Check sign-in before starting anything.
    let models = crate::catalog::load().await?;
    let plan = plan(&config, &models, model)?;
    let token = ensure_bridge(&client()?).await?;
    let mut command = std::process::Command::new("claude");
    sanitize_claude(&mut command);
    command
        .arg("--settings")
        .arg(plan.settings.to_string())
        .args(args)
        .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{}", port()))
        .env("ANTHROPIC_AUTH_TOKEN", token)
        .env("ANTHROPIC_MODEL", &plan.model)
        .env("ANTHROPIC_DEFAULT_OPUS_MODEL", &plan.model)
        .env("ANTHROPIC_DEFAULT_SONNET_MODEL", &plan.model)
        .env("ANTHROPIC_DEFAULT_HAIKU_MODEL", &plan.small_model)
        .env("ANTHROPIC_SMALL_FAST_MODEL", &plan.small_model)
        .env("CLAUDE_CODE_SUBAGENT_MODEL", &plan.model)
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        // claude.ai connectors need claude.ai auth, which the bridge token replaces; turning
        // them off also removes Claude Code's warning about it.
        .env("ENABLE_CLAUDEAI_MCP_SERVERS", "false")
        // The plan route rejects Responses tool_search.
        .env("ENABLE_TOOL_SEARCH", "false")
        .env(
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
            plan.context_tokens.to_string(),
        )
        .env(
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
            plan.context_tokens.to_string(),
        );
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
            crate::config::state_dir()?.join("bridge.log").display()
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
    fn plan_uses_catalog_defaults_and_picker_rows() {
        let models = [
            model("big", true),
            model("hidden", false),
            model("gpt-luna", true),
        ];
        let plan = plan(&crate::config::Config::default(), &models, None).unwrap();
        assert_eq!(plan.model, "big");
        assert_eq!(plan.small_model, "gpt-luna");
        assert_eq!(plan.context_tokens, 272_000);
        let rows = plan.settings["modelPicker"]["options"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["behavesAs"], "claude-opus-5-5");
        assert_eq!(plan.settings["modelPicker"]["replaceBuiltInOptions"], true);
    }

    #[test]
    fn plan_honors_requested_and_configured_models() {
        let models = [model("big", true)];
        let config = crate::config::Config {
            model: "configured".into(),
            context_tokens: 1000,
            ..Default::default()
        };
        let plan = plan(&config, &models, Some("requested".into())).unwrap();
        assert_eq!(plan.model, "requested");
        assert_eq!(plan.small_model, "requested");
        assert_eq!(plan.context_tokens, 1000);
        // Models missing from the catalog still get a picker row so Claude Code accepts them.
        let rows = plan.settings["modelPicker"]["options"].as_array().unwrap();
        assert!(rows.iter().any(|r| r["model"] == "requested"));
        assert!(super::plan(&crate::config::Config::default(), &[], None).is_err());
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
