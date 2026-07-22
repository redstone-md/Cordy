//! Update check: is a newer Cordy published than the one running?
//!
//! On startup Cordy asks crates.io (falling back to GitHub releases) for the latest version and
//! compares it against the compiled-in `CARGO_PKG_VERSION`. The result is cached to disk for a day
//! so the network is hit at most once per 24h, and any failure degrades silently to "no update".

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const CRATES_URL: &str = "https://crates.io/api/v1/crates/cordy";
const GITHUB_URL: &str = "https://api.github.com/repos/redstone-md/Cordy/releases/latest";
const MAX_AGE_SECS: u64 = 24 * 3600;
const UA: &str = concat!("cordy/", env!("CARGO_PKG_VERSION"));

#[derive(Serialize, Deserialize)]
struct CacheFile {
    checked_at: u64,
    latest: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The latest published version if it is strictly newer than the running build, else `None`.
/// Uses a ≤24h disk cache under `cache_dir`; never hard-fails.
pub async fn check(cache_dir: &Path) -> Option<String> {
    let current = env!("CARGO_PKG_VERSION");
    let path = cache_dir.join("update_check.json");

    let cached: Option<CacheFile> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    let latest = match &cached {
        Some(cf) if now_secs().saturating_sub(cf.checked_at) < MAX_AGE_SECS => cf.latest.clone(),
        _ => {
            let fetched = fetch_latest().await?;
            let _ = std::fs::create_dir_all(cache_dir);
            if let Ok(json) = serde_json::to_string(&CacheFile {
                checked_at: now_secs(),
                latest: fetched.clone(),
            }) {
                let _ = std::fs::write(&path, json);
            }
            fetched
        }
    };

    if is_newer(&latest, current) {
        Some(latest)
    } else {
        None
    }
}

/// crates.io first, then GitHub releases; returns the bare version (no leading `v`).
async fn fetch_latest() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .user_agent(UA)
        .build()
        .ok()?;

    if let Ok(resp) = client.get(CRATES_URL).send().await
        && let Ok(resp) = resp.error_for_status()
        && let Ok(v) = resp.json::<Value>().await
        && let Some(ver) = v
            .get("crate")
            .and_then(|c| c.get("max_stable_version"))
            .and_then(|s| s.as_str())
    {
        return Some(ver.to_string());
    }

    let v: Value = client
        .get(GITHUB_URL)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    let tag = v.get("tag_name").and_then(|s| s.as_str())?;
    Some(tag.trim_start_matches('v').to_string())
}

/// True when `latest` parses as a semver strictly greater than `current`.
fn is_newer(latest: &str, current: &str) -> bool {
    match (
        semver::Version::parse(latest.trim_start_matches('v')),
        semver::Version::parse(current),
    ) {
        (Ok(l), Ok(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_detected() {
        assert!(is_newer("0.1.2", "0.1.1"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn same_or_older_is_not_newer() {
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("garbage", "0.1.1"));
    }
}
