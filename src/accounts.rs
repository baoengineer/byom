//! `login`, `logout` and `auth`: signing in to providers and reporting sign-in status.
use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::providers::{Auth, Provider};

/// How a provider is (or is not) usable right now.
pub fn status(
    config: &crate::config::Config,
    provider: &Provider,
    claude_signed_in: bool,
) -> String {
    let configured_key = config
        .providers
        .get(&provider.id)
        .map(|c| c.api_key.as_str())
        .filter(|k| !k.is_empty());
    match (provider.auth, configured_key) {
        (_, Some(reference)) if reference.starts_with('$') => format!("key from {reference}"),
        (_, Some(reference)) if reference.starts_with('!') => "key from command".into(),
        (_, Some(_)) => "key in config".into(),
        (Auth::ClaudeCode, None) if claude_signed_in => "via Claude Code sign-in".into(),
        (Auth::ClaudeCode, None) => "Claude Code not signed in".into(),
        (Auth::ChatGpt, None) => match crate::store::auth::get(&provider.id) {
            Ok(Some(_)) => "signed in".into(),
            _ => "not signed in".into(),
        },
        (Auth::ApiKey, None) => match crate::store::auth::get(&provider.id) {
            Ok(Some(v)) if v["key"].is_string() => "key saved".into(),
            _ => "no key".into(),
        },
        (Auth::None, None) => "local, no sign-in".into(),
    }
}

fn read_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

fn read_secret(prompt: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        rpassword::prompt_password(prompt).context("reading key")
    } else {
        // Piped input: `printf %s "$KEY" | byoclaude login zai`.
        read_line("")
    }
}

/// A pasted key, stored as typed: never resolved as a `$VAR` or `!command` reference.
fn pasted_key(input: &str) -> Result<Option<String>> {
    let key = input.trim();
    if key.chars().any(char::is_control) {
        bail!("API key contains control characters");
    }
    Ok((!key.is_empty()).then(|| key.to_owned()))
}

fn choose(config: &crate::config::Config) -> Result<Provider> {
    let providers = crate::providers::all(config);
    if !std::io::stdin().is_terminal() {
        bail!(
            "name a provider: byoclaude login <provider>. Providers: {}",
            ids(&providers)
        );
    }
    println!("Sign in to a provider:\n");
    for (i, p) in providers.iter().enumerate() {
        println!("  {:>2}. {:<11} {}", i + 1, p.id, p.name);
    }
    let answer = read_line("\nNumber or provider ID: ")?;
    let picked = answer
        .parse::<usize>()
        .ok()
        .and_then(|n| providers.get(n.wrapping_sub(1)).cloned())
        .or_else(|| providers.iter().find(|p| p.id == answer).cloned());
    picked.with_context(|| format!("no provider {answer:?}"))
}

fn ids(providers: &[Provider]) -> String {
    providers
        .iter()
        .map(|p| p.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub async fn login(provider: Option<String>) -> Result<()> {
    let config = crate::config::load()?;
    let provider = match provider {
        // 0.1.0 accepted `login chatgpt`.
        Some(id) if id == "chatgpt" => {
            crate::providers::find(&config, "openai").context("openai provider is disabled")?
        }
        Some(id) => crate::providers::find(&config, &id).with_context(|| {
            format!(
                "unknown provider {id:?}. Providers: {}",
                ids(&crate::providers::all(&config))
            )
        })?,
        None => choose(&config)?,
    };
    match provider.auth {
        Auth::ChatGpt => crate::auth::login().await?,
        Auth::None => {
            println!("{} needs no sign-in. {}", provider.name, provider.note);
        }
        Auth::ClaudeCode | Auth::ApiKey => {
            if provider.auth == Auth::ClaudeCode {
                println!(
                    "Claude models use Claude Code's own sign-in (run `claude auth login`).\nTo use an Anthropic API key instead, paste it below; leave it empty to cancel."
                );
            } else {
                println!(
                    "{}\n{}\nGet a key: {}",
                    provider.name, provider.note, provider.signup
                );
            }
            let key = read_secret("API key: ")?;
            let Some(key) = pasted_key(&key)? else {
                println!("No key saved.");
                return Ok(());
            };
            let _lock = crate::store::lock().await?;
            crate::store::auth::set(&provider.id, json!({"key": key}))?;
            println!("Key saved for {}.", provider.id);
        }
    }
    let roster = crate::catalog::refresh(&crate::config::load()?).await;
    let count = roster
        .iter()
        .filter(|e| e.provider == provider.id && e.listed)
        .count();
    if count > 0 {
        println!(
            "{count} {} model(s) available. List them with: byoclaude models --provider {}",
            provider.id, provider.id
        );
    } else if provider.auth != Auth::ClaudeCode {
        println!(
            "No {} models found yet. Name the ones you want in config: byoclaude config set providers.{}.models <model>",
            provider.id, provider.id
        );
    }
    Ok(())
}

pub async fn logout(provider: String) -> Result<()> {
    let _lock = crate::store::lock().await?;
    let removed = crate::store::auth::remove(&provider)?;
    drop(_lock);
    if removed {
        println!("Signed out of {provider}.");
        crate::catalog::refresh(&crate::config::load()?).await;
    } else {
        println!("No saved sign-in for {provider}.");
    }
    if provider == "openai" {
        println!("To revoke byoclaude's access entirely, disconnect it in ChatGPT settings.");
    }
    Ok(())
}

pub fn print_status() -> Result<()> {
    let config = crate::config::load()?;
    let claude = crate::launch::claude_signed_in();
    let providers = crate::providers::all(&config);
    let roster = crate::catalog::roster_cached();
    println!("{:<11} {:<26} {:<28} MODELS", "PROVIDER", "NAME", "STATUS");
    for provider in &providers {
        let models = roster.iter().filter(|e| e.provider == provider.id).count();
        println!(
            "{:<11} {:<26} {:<28} {}",
            provider.id,
            provider.name,
            status(&config, provider, claude),
            if models == 0 {
                "-".into()
            } else {
                models.to_string()
            }
        );
    }
    println!(
        "\nClaude relay: {}",
        match (config.relay, claude) {
            (true, true) => "on (Claude Code is signed in)",
            (true, false) => "inactive (Claude Code is not signed in)",
            (false, _) => "off in config",
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_keys_are_stored_literally() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let command = format!("!touch '{}'", marker.display());
        assert_eq!(pasted_key(&command).unwrap(), Some(command.clone()));
        assert!(!marker.exists());
        assert_eq!(pasted_key(" sk-1\n").unwrap().as_deref(), Some("sk-1"));
        assert_eq!(pasted_key("  ").unwrap(), None);
        assert!(pasted_key("sk\u{7}1").is_err());
    }
}
