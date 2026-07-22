//! Model metadata from [models.dev](https://models.dev) — context window, pricing, capabilities.
//!
//! Cordy fetches the catalog once and caches it to disk (refreshed weekly), so it can show
//! context sizes and prices in the model picker and compute cost. Config `[[model]]` entries
//! override the fetched values.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const API_URL: &str = "https://models.dev/api.json";
const MAX_AGE_SECS: u64 = 7 * 24 * 3600;

/// Metadata for a single model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    /// Context window in tokens.
    pub context: Option<u64>,
    /// USD price per million input / output tokens.
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    pub tool_call: bool,
    pub reasoning: bool,
}

/// A lookup of model id -> [`ModelInfo`].
#[derive(Default)]
pub struct Catalog {
    by_id: HashMap<String, ModelInfo>,
}

impl Catalog {
    pub fn from_models(models: Vec<ModelInfo>) -> Self {
        let by_id = models.into_iter().map(|m| (m.id.clone(), m)).collect();
        Catalog { by_id }
    }

    /// Look up a model by exact id, then by trailing path segment (`meta/llama-x` -> `llama-x`).
    pub fn get(&self, id: &str) -> Option<&ModelInfo> {
        self.by_id
            .get(id)
            .or_else(|| id.rsplit('/').next().and_then(|tail| self.by_id.get(tail)))
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    fetched_at: u64,
    models: Vec<ModelInfo>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Flatten the models.dev `provider -> {models: {id -> model}}` JSON into a flat model list.
pub fn parse_catalog(v: &Value) -> Vec<ModelInfo> {
    let mut out = Vec::new();
    let Some(providers) = v.as_object() else {
        return out;
    };
    for prov in providers.values() {
        let provider = prov["name"].as_str().unwrap_or_default().to_string();
        let Some(models) = prov["models"].as_object() else {
            continue;
        };
        for (mid, m) in models {
            out.push(ModelInfo {
                id: mid.clone(),
                name: m["name"].as_str().unwrap_or(mid).to_string(),
                provider: provider.clone(),
                context: m["limit"]["context"].as_u64(),
                price_in: m["cost"]["input"].as_f64(),
                price_out: m["cost"]["output"].as_f64(),
                tool_call: m["tool_call"].as_bool().unwrap_or(false),
                reasoning: m["reasoning"].as_bool().unwrap_or(false),
            });
        }
    }
    out
}

/// Load the catalog: use a fresh on-disk cache, else fetch from models.dev and cache it. On any
/// network failure a stale cache (or empty catalog) is used, so startup never hard-fails.
pub async fn load_catalog(cache_dir: &Path) -> Catalog {
    let path = cache_dir.join("models_dev.json");
    let cached: Option<CacheFile> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    if let Some(cf) = &cached
        && now_secs().saturating_sub(cf.fetched_at) < MAX_AGE_SECS
    {
        return Catalog::from_models(cf.models.clone());
    }

    match fetch().await {
        Ok(models) => {
            let _ = std::fs::create_dir_all(cache_dir);
            if let Ok(json) = serde_json::to_string(&CacheFile {
                fetched_at: now_secs(),
                models: models.clone(),
            }) {
                let _ = std::fs::write(&path, json);
            }
            Catalog::from_models(models)
        }
        Err(_) => Catalog::from_models(cached.map(|c| c.models).unwrap_or_default()),
    }
}

async fn fetch() -> anyhow::Result<Vec<ModelInfo>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()?;
    let v: Value = client
        .get(API_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(parse_catalog(&v))
}

/// Human-friendly context size, e.g. `128k`.
pub fn fmt_context(ctx: u64) -> String {
    if ctx >= 1_000_000 {
        format!("{}M", ctx / 1_000_000)
    } else if ctx >= 1_000 {
        format!("{}k", ctx / 1_000)
    } else {
        ctx.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_provider_models() {
        let v = json!({
            "mistral": {
                "id": "mistral", "name": "Mistral",
                "models": {
                    "mistral-small-latest": {
                        "id": "mistral-small-latest", "name": "Mistral Small",
                        "tool_call": true, "reasoning": true,
                        "limit": { "context": 256000, "output": 256000 },
                        "cost": { "input": 0.15, "output": 0.6 }
                    }
                }
            }
        });
        let cat = Catalog::from_models(parse_catalog(&v));
        let m = cat.get("mistral-small-latest").unwrap();
        assert_eq!(m.provider, "Mistral");
        assert_eq!(m.context, Some(256000));
        assert_eq!(m.price_in, Some(0.15));
        assert!(m.tool_call);
    }

    #[test]
    fn get_matches_trailing_segment() {
        let v = json!({
            "meta": { "name": "Meta", "models": {
                "llama-3.1-8b-instruct": { "id": "llama-3.1-8b-instruct", "name": "Llama 3.1 8B",
                    "limit": { "context": 128000 }, "cost": {} }
            }}
        });
        let cat = Catalog::from_models(parse_catalog(&v));
        // Cordy uses the namespaced id; catalog keys the bare id.
        assert!(cat.get("meta/llama-3.1-8b-instruct").is_some());
    }

    #[test]
    fn formats_context() {
        assert_eq!(fmt_context(128000), "128k");
        assert_eq!(fmt_context(2_000_000), "2M");
        assert_eq!(fmt_context(512), "512");
    }
}
