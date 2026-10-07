//! The signed-in account's model catalog, cached locally for the launcher and bridge.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MODELS_URL: &str = "https://api.openai.com/v1/models";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Model {
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub context_window: u64,
    pub effort_levels: Vec<String>,
    pub default_effort: Option<String>,
    pub listed: bool,
}

fn cache_path() -> Result<std::path::PathBuf> {
    Ok(crate::store::cache_dir()?.join("models.json"))
}

pub fn parse(catalog: &Value) -> Result<Vec<Model>> {
    let models = catalog["models"]
        .as_array()
        .context("unexpected ChatGPT model catalog")?;
    Ok(models
        .iter()
        .filter_map(|m| {
            Some(Model {
                slug: m["slug"].as_str()?.to_owned(),
                display_name: m["display_name"].as_str().unwrap_or("").to_owned(),
                description: m["description"].as_str().unwrap_or("").to_owned(),
                context_window: m["context_window"].as_u64().unwrap_or(0),
                effort_levels: m["supported_reasoning_levels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l["effort"].as_str().map(str::to_owned))
                    .collect(),
                default_effort: m["default_reasoning_level"].as_str().map(str::to_owned),
                listed: m["visibility"] == "list",
            })
        })
        .collect())
}

/// Fetch the catalog for the signed-in account and refresh the local cache.
pub async fn fetch() -> Result<Vec<Model>> {
    let token = crate::auth::access_token().await?;
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(MODELS_URL)
        .bearer_auth(&token)
        .send()
        .await
        .context("fetching ChatGPT model catalog")?;
    if !response.status().is_success() {
        bail!(
            "ChatGPT model list failed (HTTP {})",
            response.status().as_u16()
        );
    }
    let models = parse(&response.json().await?)?;
    if let Ok(path) = cache_path() {
        let _ = crate::store::write_private(&path, &serde_json::to_vec_pretty(&models)?);
    }
    Ok(models)
}

pub fn cached() -> Option<Vec<Model>> {
    serde_json::from_slice(&std::fs::read(cache_path().ok()?).ok()?).ok()
}

/// Cached catalog, fetching it when no cache exists.
pub async fn load() -> Result<Vec<Model>> {
    match cached() {
        Some(models) if !models.is_empty() => Ok(models),
        _ => fetch().await,
    }
}

pub fn find<'a>(models: &'a [Model], slug: &str) -> Option<&'a Model> {
    models.iter().find(|m| m.slug == slug)
}

pub async fn print() -> Result<()> {
    for model in fetch().await?.iter().filter(|m| m.listed) {
        println!(
            "{}\t{}\t{}k context\t{}",
            model.slug,
            model.display_name,
            model.context_window / 1000,
            model.effort_levels.join("/")
        );
    }
    Ok(())
}

/// A model any provider offers, with metadata for the picker, the skill and compaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Entry {
    /// Routable ID: `provider/model`, or a native Claude ID.
    pub id: String,
    pub provider: String,
    pub name: String,
    pub context_window: u64,
    pub max_output: u64,
    pub reasoning: bool,
    pub tools: bool,
    pub images: bool,
    /// USD per million tokens, from models.dev; 0 when unknown.
    pub cost_input: f64,
    pub cost_output: f64,
    pub effort_levels: Vec<String>,
    /// Offered in /model; very large provider catalogs offer only configured models.
    pub listed: bool,
}

/// Providers listing more models than this offer only those named in config.
const LISTED_LIMIT: usize = 25;
const MODELS_DEV_URL: &str = "https://models.dev/api.json";

fn roster_path() -> Result<std::path::PathBuf> {
    Ok(crate::store::cache_dir()?.join("roster.json"))
}

/// Whether a roster has been built since this home was created.
pub fn roster_exists() -> bool {
    roster_path().is_ok_and(|p| p.exists())
}

pub fn roster_cached() -> Vec<Entry> {
    roster_path()
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// models.dev data, refreshed at most daily.
async fn models_dev(client: &reqwest::Client) -> Value {
    let Ok(path) = crate::store::cache_dir().map(|d| d.join("models.dev.json")) else {
        return Value::Null;
    };
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age.as_secs() < 86_400));
    if !fresh
        && let Ok(response) = client.get(MODELS_DEV_URL).send().await
        && response.status().is_success()
        && let Ok(bytes) = response.bytes().await
        && serde_json::from_slice::<Value>(&bytes).is_ok()
    {
        let _ = crate::store::write_private(&path, &bytes);
    }
    std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

fn enrich(entry: &mut Entry, info: &Value) {
    if info.is_null() {
        return;
    }
    if entry.name.is_empty() {
        entry.name = info["name"].as_str().unwrap_or("").to_owned();
    }
    if entry.context_window == 0 {
        entry.context_window = info["limit"]["context"].as_u64().unwrap_or(0);
    }
    entry.max_output = info["limit"]["output"].as_u64().unwrap_or(entry.max_output);
    entry.reasoning |= info["reasoning"].as_bool().unwrap_or(false);
    entry.tools = info["tool_call"].as_bool().unwrap_or(entry.tools);
    entry.images |= info["modalities"]["input"]
        .as_array()
        .is_some_and(|i| i.iter().any(|m| m == "image"));
    entry.cost_input = info["cost"]["input"].as_f64().unwrap_or(entry.cost_input);
    entry.cost_output = info["cost"]["output"].as_f64().unwrap_or(entry.cost_output);
}

/// Model IDs a provider serves, from its OpenAI- or Anthropic-style `/v1/models`.
async fn discover(
    client: &reqwest::Client,
    provider: &crate::providers::Provider,
    key: Option<&str>,
) -> Option<Vec<String>> {
    let base = provider.base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    };
    let mut request = client.get(url).timeout(std::time::Duration::from_secs(8));
    if let Some(key) = key {
        request = request.bearer_auth(key).header("x-api-key", key);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    let ids: Vec<String> = body["data"]
        .as_array()?
        .iter()
        .filter_map(|m| m["id"].as_str().map(str::to_owned))
        .collect();
    (!ids.is_empty()).then_some(ids)
}

/// Rebuild the roster from every provider that is usable now, and cache it.
pub async fn refresh(config: &crate::config::Config) -> Vec<Entry> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let dev = models_dev(&client).await;
    let mut roster = Vec::new();
    for provider in crate::providers::all(config) {
        use crate::providers::Auth;
        let key = crate::providers::api_key(config, &provider).ok().flatten();
        let dev_models = &dev[provider.models_dev.as_str()]["models"];
        let mut entries: Vec<Entry> = match provider.auth {
            Auth::ChatGpt => {
                let models = match fetch().await {
                    Ok(models) => models,
                    Err(_) => cached().unwrap_or_default(),
                };
                models
                    .into_iter()
                    .map(|m| Entry {
                        id: crate::providers::canonical(&m.slug),
                        provider: provider.id.clone(),
                        name: m.display_name,
                        context_window: m.context_window,
                        reasoning: !m.effort_levels.is_empty(),
                        tools: true,
                        images: true,
                        effort_levels: m.effort_levels,
                        listed: m.listed,
                        ..Default::default()
                    })
                    .collect()
            }
            Auth::ClaudeCode => dev_models
                .as_object()
                .into_iter()
                .flatten()
                .map(|(id, _)| Entry {
                    id: id.clone(),
                    provider: provider.id.clone(),
                    listed: false,
                    ..Default::default()
                })
                .collect(),
            Auth::ApiKey if key.is_none() => Vec::new(),
            Auth::ApiKey | Auth::None => {
                let mut ids = discover(&client, &provider, key.as_deref())
                    .await
                    .unwrap_or_default();
                if ids.is_empty() && provider.auth == Auth::ApiKey {
                    ids = dev_models
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter(|(_, info)| info["tool_call"].as_bool() != Some(false))
                        .map(|(id, _)| id.clone())
                        .collect();
                }
                let configured = &provider.models;
                let crowded = ids.len() > LISTED_LIMIT;
                for model in configured {
                    if !ids.contains(model) {
                        ids.push(model.clone());
                    }
                }
                ids.into_iter()
                    .map(|model| Entry {
                        id: format!("{}/{model}", provider.id),
                        provider: provider.id.clone(),
                        listed: !crowded || configured.contains(&model),
                        tools: true,
                        ..Default::default()
                    })
                    .collect()
            }
        };
        for entry in &mut entries {
            let (_, name) = crate::providers::split(&entry.id);
            enrich(entry, &dev_models[name]);
        }
        roster.extend(entries);
    }
    if let Ok(path) = roster_path() {
        let _ = crate::store::write_private(
            &path,
            &serde_json::to_vec_pretty(&roster).unwrap_or_default(),
        );
    }
    roster
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_catalog_fields() {
        let models = super::parse(&serde_json::json!({"models": [
            {"slug": "a", "display_name": "A", "visibility": "list", "context_window": 272000,
             "supported_reasoning_levels": [{"effort": "low"}, {"effort": "max"}], "default_reasoning_level": "low"},
            {"slug": "hidden", "visibility": "hide"},
            {"no_slug": true},
        ]}))
        .unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].effort_levels, ["low", "max"]);
        assert!(!models[1].listed);
    }
}
