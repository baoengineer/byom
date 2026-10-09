//! `byom doctor`: check everything a session depends on and say how to fix what fails.
use anyhow::Result;

use crate::providers::Auth;

struct Report {
    failures: usize,
}

impl Report {
    fn line(&mut self, ok: bool, what: &str, detail: &str) {
        if !ok {
            self.failures += 1;
        }
        let mark = if ok { "ok  " } else { "FAIL" };
        if detail.is_empty() {
            println!("{mark} {what}");
        } else {
            println!("{mark} {what}: {detail}");
        }
    }

    fn note(&self, what: &str, detail: &str) {
        println!("--   {what}: {detail}");
    }
}

pub async fn run() -> Result<()> {
    let mut report = Report { failures: 0 };

    let version = std::process::Command::new("claude")
        .arg("--version")
        .output();
    match version {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            report.line(true, "Claude Code installed", &text);
        }
        _ => report.line(
            false,
            "Claude Code installed",
            "`claude` not found on PATH; install it from https://claude.com/claude-code",
        ),
    }

    let config = match crate::config::load() {
        Ok(config) => {
            report.line(
                true,
                "Config",
                &crate::store::config_path()?.display().to_string(),
            );
            config
        }
        Err(e) => {
            report.line(
                false,
                "Config",
                &format!("{e:#}; fix it with `byom config`"),
            );
            crate::config::Config::default()
        }
    };

    let claude = crate::launch::claude_signed_in();
    match (config.relay, claude) {
        (true, true) => report.line(
            true,
            "Claude relay",
            "on; Claude models use Claude Code's own sign-in",
        ),
        (true, false) => report.note(
            "Claude relay",
            "inactive; Claude Code is not signed in (`claude auth login` to add Claude models)",
        ),
        (false, _) => report.note("Claude relay", "off in config"),
    }

    match crate::launch::bridge_version().await {
        Some(v) if v == env!("CARGO_PKG_VERSION") => {
            report.line(true, "Bridge", &format!("running {v}"))
        }
        Some(v) => report.note(
            "Bridge",
            &format!(
                "running {v}; the next `byom run` replaces it with {}",
                env!("CARGO_PKG_VERSION")
            ),
        ),
        None => report.note("Bridge", "not running; `byom run` starts it"),
    }

    let client = crate::catalog::client(8)?;
    let mut usable = 0;
    for provider in crate::providers::all(&config) {
        let status = crate::accounts::status(&config, &provider, claude);
        match provider.auth {
            Auth::ChatGpt => {
                if crate::store::auth::get(&provider.id)
                    .ok()
                    .flatten()
                    .is_none()
                {
                    report.note(&provider.id, "not signed in (`byom login openai`)");
                    continue;
                }
                match crate::auth::access_token().await {
                    Ok(_) => {
                        usable += 1;
                        report.line(true, &provider.id, "ChatGPT sign-in valid")
                    }
                    Err(e) => report.line(
                        false,
                        &provider.id,
                        &format!("{e:#}; run `byom login openai`"),
                    ),
                }
            }
            Auth::ClaudeCode => {
                if claude && config.relay {
                    usable += 1;
                }
            }
            Auth::ApiKey => {
                let key = crate::providers::api_key(&config, &provider);
                match key {
                    Ok(Some(key)) => {
                        let reachable =
                            reach(&client, &crate::catalog::models_url(&provider), Some(&key))
                                .await;
                        if reachable {
                            usable += 1;
                        }
                        report.line(
                            reachable,
                            &provider.id,
                            &format!(
                                "{status}; {}",
                                if reachable {
                                    "reachable"
                                } else {
                                    "not reachable or key rejected"
                                }
                            ),
                        );
                    }
                    Ok(None) => {}
                    Err(e) => report.line(false, &provider.id, &format!("{e:#}")),
                }
            }
            Auth::None => {
                if reach(&client, &crate::catalog::models_url(&provider), None).await {
                    usable += 1;
                    report.line(true, &provider.id, "local server running");
                }
            }
        }
    }
    report.line(usable > 0, "Usable providers", &usable.to_string());

    let capped = crate::roster::capped(&crate::roster::log_entries());
    for (model, outcome) in &capped {
        let short: String = outcome.chars().take(100).collect();
        report.note(&format!("{model} limited recently"), &short);
    }

    if report.failures == 0 {
        println!("\nAll checks passed.");
        Ok(())
    } else {
        anyhow::bail!("{} check(s) failed", report.failures)
    }
}

/// Whether a provider's model list answers; any HTTP answer below 500 other than 401/403
/// counts as reachable.
async fn reach(client: &reqwest::Client, url: &str, key: Option<&str>) -> bool {
    let mut request = client.get(url);
    if let Some(key) = key {
        request = request.bearer_auth(key).header("x-api-key", key);
    }
    match request.send().await {
        Ok(response) => {
            let code = response.status().as_u16();
            code < 500 && code != 401 && code != 403
        }
        Err(_) => false,
    }
}
