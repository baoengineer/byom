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
    Ok(crate::config::state_dir()?.join("models.json"))
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
        let _ = crate::auth::write_private(&path, &serde_json::to_vec_pretty(&models)?);
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
