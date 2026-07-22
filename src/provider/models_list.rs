//! Fetch the live model list from an OpenAI-compatible `/models` endpoint.

use std::time::Duration;

use serde_json::Value;

/// GET `{base_url}/models` (bearer auth) and return the sorted list of model ids. Works for any
/// OpenAI-compatible endpoint (OpenAI, NVIDIA, ollama, vllm, ...).
pub async fn list_models(base_url: &str, api_key: &str) -> anyhow::Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()?;
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client.get(url);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let v: Value = req.send().await?.error_for_status()?.json().await?;
    let mut ids: Vec<String> = v["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids.dedup();
    Ok(ids)
}
