//! `byoclaude models` and `byoclaude logs`: the model roster with live status, and the
//! request log.
use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Value, json};

use crate::catalog::Entry;
use crate::providers::{Auth, Provider};

/// A plan or rate limit within this window marks a model as capped.
const CAPPED_SECONDS: u64 = 5 * 3600;

/// One bridge log line.
pub fn log_entries() -> Vec<Value> {
    let Ok(path) = crate::store::log_path() else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Models whose latest request in the window hit a plan or rate limit.
pub fn capped(log: &[Value]) -> HashMap<String, String> {
    let mut latest: HashMap<String, (u64, String)> = HashMap::new();
    for entry in log {
        let (Some(model), Some(at)) = (entry["model"].as_str(), entry["at"].as_u64()) else {
            continue;
        };
        if now().saturating_sub(at) > CAPPED_SECONDS {
            continue;
        }
        let outcome = entry["outcome"].as_str().unwrap_or("").to_owned();
        latest.insert(crate::providers::canonical(model), (at, outcome));
    }
    latest
        .into_iter()
        .filter(|(_, (_, outcome))| outcome.contains("limit") || outcome.contains("HTTP 429"))
        .map(|(model, (_, outcome))| (model, outcome))
        .collect()
}

fn provider_ready(
    config: &crate::config::Config,
    provider: &Provider,
    claude: bool,
) -> &'static str {
    match provider.auth {
        Auth::ClaudeCode if claude && config.relay => "ready",
        Auth::ClaudeCode
            if crate::providers::api_key(config, provider)
                .ok()
                .flatten()
                .is_some() =>
        {
            "ready"
        }
        Auth::ClaudeCode => "signed-out",
        Auth::ChatGpt
            if crate::store::auth::get(&provider.id)
                .ok()
                .flatten()
                .is_some() =>
        {
            "ready"
        }
        Auth::ChatGpt => "signed-out",
        Auth::ApiKey
            if crate::providers::api_key(config, provider)
                .ok()
                .flatten()
                .is_some() =>
        {
            "ready"
        }
        Auth::ApiKey => "no-key",
        Auth::None => "ready",
    }
}

/// The roster as Claude and scripts see it.
pub fn roster_json(
    config: &crate::config::Config,
    roster: &[Entry],
    include_hidden: bool,
) -> Value {
    let providers = crate::providers::all(config);
    let claude = crate::launch::claude_signed_in();
    let log = log_entries();
    let capped = capped(&log);
    let models: Vec<Value> = roster
        .iter()
        .filter(|e| include_hidden || e.listed || e.provider == crate::providers::CLAUDE_PROVIDER)
        .filter_map(|e| {
            let provider = providers.iter().find(|p| p.id == e.provider)?;
            let mut status = provider_ready(config, provider, claude);
            if status == "ready" && capped.contains_key(&e.id) {
                status = "capped";
            }
            Some(json!({
                "id": e.id,
                "provider": e.provider,
                "provider_name": provider.name,
                "name": e.name,
                "agent": if crate::providers::is_claude(&e.id) { Value::Null } else { json!(crate::skill::agent_type(&e.id)) },
                "status": status,
                "context_window": e.context_window,
                "max_output": e.max_output,
                "reasoning": e.reasoning,
                "effort_levels": e.effort_levels,
                "tools": e.tools,
                "images": e.images,
                "price_per_million": {"input": e.cost_input, "output": e.cost_output},
                "in_model_picker": e.listed,
            }))
        })
        .collect();
    let plan = crate::launch::plan(
        config,
        &crate::catalog::cached().unwrap_or_default(),
        roster,
        None,
        config.relay && claude,
    )
    .ok();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "relay": config.relay && claude,
        "roles": {
            "main": plan.as_ref().and_then(|p| p.model.clone()),
            "background": plan.as_ref().and_then(|p| p.background.clone()),
            "subagent": plan.as_ref().and_then(|p| p.subagent.clone()),
        },
        "models": models,
    })
}

pub async fn print(
    json_output: bool,
    provider: Option<String>,
    refresh: bool,
    all: bool,
) -> Result<()> {
    let config = crate::config::load()?;
    let mut roster = crate::catalog::roster_cached();
    if refresh || roster.is_empty() {
        roster = crate::catalog::refresh(&config).await;
    }
    if let Some(id) = &provider {
        roster.retain(|e| &e.provider == id);
    }
    let view = roster_json(&config, &roster, all);
    if json_output {
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }
    let mut models = view["models"].as_array().cloned().unwrap_or_default();
    // Usable models first, then by provider order.
    models.sort_by_key(|m| m["status"] != "ready");
    if models.is_empty() {
        println!(
            "No models yet. Sign in with `byoclaude login`, then run `byoclaude models --refresh`."
        );
        return Ok(());
    }
    println!(
        "{:<42} {:>8} {:>16}  STATUS",
        "MODEL", "CONTEXT", "$/M IN/OUT"
    );
    for m in &models {
        let context = m["context_window"].as_u64().unwrap_or(0);
        let price = &m["price_per_million"];
        let (input, output) = (
            price["input"].as_f64().unwrap_or(0.0),
            price["output"].as_f64().unwrap_or(0.0),
        );
        println!(
            "{:<42} {:>8} {:>16}  {}{}",
            m["id"].as_str().unwrap_or(""),
            if context == 0 {
                "-".into()
            } else {
                format!("{}k", context / 1000)
            },
            if input == 0.0 && output == 0.0 {
                "-".into()
            } else {
                format!("{input:.2}/{output:.2}")
            },
            m["status"].as_str().unwrap_or(""),
            if m["in_model_picker"] == false
                && !m["id"].as_str().unwrap_or("").starts_with("claude")
            {
                " (hidden)"
            } else {
                ""
            }
        );
    }
    if !all
        && roster
            .iter()
            .any(|e| !e.listed && e.provider != crate::providers::CLAUDE_PROVIDER)
    {
        println!(
            "\nSome models are hidden from /model; show them with --all, add them with providers.<id>.models."
        );
    }
    Ok(())
}

pub fn print_logs(lines: usize) -> Result<()> {
    let log = log_entries();
    if log.is_empty() {
        println!(
            "No requests logged yet ({}).",
            crate::store::log_path()?.display()
        );
        return Ok(());
    }
    println!(
        "{:<8} {:<34} {:<9} {:>7} {:>8} {:>15}  OUTCOME",
        "TIME", "MODEL", "ROUTE", "FIRST", "TOTAL", "TOKENS IN/OUT"
    );
    for entry in &log[log.len().saturating_sub(lines)..] {
        let at = entry["at"].as_u64().unwrap_or(0);
        let time = format!(
            "{:02}:{:02}:{:02}",
            (at / 3600) % 24,
            (at / 60) % 60,
            at % 60
        );
        let usage = &entry["usage"];
        let tokens = match (
            usage["input_tokens"].as_u64(),
            usage["output_tokens"].as_u64(),
        ) {
            (Some(i), Some(o)) => format!(
                "{}/{o}",
                i + usage["cache_read_input_tokens"].as_u64().unwrap_or(0)
            ),
            _ => "-".into(),
        };
        let route = match entry["transport"].as_str() {
            Some("websocket") if entry["continued"] == true => "ws+cont",
            Some(t) => t,
            None => "-",
        };
        let ms = |v: &Value| {
            v.as_u64()
                .map(|n| format!("{n}ms"))
                .unwrap_or_else(|| "-".into())
        };
        let outcome: String = entry["outcome"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        println!(
            "{time:<8} {:<34} {route:<9} {:>7} {:>8} {tokens:>15}  {outcome}",
            entry["model"].as_str().unwrap_or(""),
            ms(&entry["first_token_ms"]),
            ms(&entry["total_ms"]),
        );
    }
    println!(
        "\nTimes are UTC. Full log: {}",
        crate::store::log_path()?.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_uses_latest_recent_outcome() {
        let t = now();
        let log = vec![
            json!({"model": "gpt-a", "at": t - 100, "outcome": "ChatGPT plan limit reached"}),
            json!({"model": "openai/gpt-b", "at": t - 100, "outcome": "ChatGPT plan limit reached"}),
            json!({"model": "openai/gpt-b", "at": t - 10, "outcome": "ok"}),
            json!({"model": "kimi/k3", "at": t - CAPPED_SECONDS - 10, "outcome": "HTTP 429"}),
        ];
        let capped = capped(&log);
        assert!(capped.contains_key("openai/gpt-a"));
        assert!(!capped.contains_key("openai/gpt-b"));
        assert!(!capped.contains_key("kimi/k3"));
    }
}
