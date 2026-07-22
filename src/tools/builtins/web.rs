//! `web_search` and `web_fetch` — give the agent live web access.
//!
//! Both are read-only (never permission-prompt). `web_search` uses Exa when `EXA_API_KEY` is set
//! (richer results), otherwise it falls back to a free, key-less DuckDuckGo HTML scrape — so
//! search works out of the box. `web_fetch` retrieves a URL's text.

use async_trait::async_trait;
use regex::RegexBuilder;
use serde_json::{Value, json};

use crate::core::types::ToolOutput;
use crate::tools::{Risk, Tool, ToolCtx};

/// A common browser User-Agent so search/fetch endpoints don't serve a bot page.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/122.0 Safari/537.36";

/// Exa `/search` request body.
fn exa_body(query: &str, num: u64) -> Value {
    json!({
        "query": query,
        "numResults": num,
        "contents": { "text": { "maxCharacters": 2000 } }
    })
}

/// Render Exa results into a compact text block.
fn format_results(v: &Value) -> String {
    let Some(arr) = v["results"].as_array() else {
        return "no results".to_string();
    };
    if arr.is_empty() {
        return "no results".to_string();
    }
    arr.iter()
        .map(|r| {
            let title = r["title"].as_str().unwrap_or("(untitled)");
            let url = r["url"].as_str().unwrap_or("");
            let text = r["text"]
                .as_str()
                .or_else(|| r["summary"].as_str())
                .unwrap_or("");
            let snippet: String = text.chars().take(600).collect();
            format!("{title}\n{url}\n{snippet}")
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// Strip HTML tags and decode a handful of common entities from a snippet/title.
fn clean_html(s: &str) -> String {
    let no_tags = RegexBuilder::new("<[^>]+>")
        .build()
        .map(|re| re.replace_all(s, "").into_owned())
        .unwrap_or_else(|_| s.to_string());
    no_tags
        .replace("&amp;", "&")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// DuckDuckGo wraps result links as `//duckduckgo.com/l/?uddg=<encoded target>`; unwrap it.
fn decode_ddg_url(href: &str) -> String {
    let full = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    if let Ok(u) = url::Url::parse(&full) {
        for (k, v) in u.query_pairs() {
            if k == "uddg" {
                return v.into_owned();
            }
        }
    }
    href.to_string()
}

/// Free, key-less web search: scrape DuckDuckGo's HTML results (POST, the method that isn't
/// bot-gated), falling back to DuckDuckGo's Instant Answer JSON API when the scrape yields nothing.
async fn ddg_search(query: &str, num: usize) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .build()?;

    if let Some(text) = ddg_html(&client, query, num).await
        && !text.is_empty()
    {
        return Ok(text);
    }
    if let Some(text) = ddg_instant(&client, query, num).await {
        return Ok(text);
    }
    Ok("no results".to_string())
}

/// Scrape the DuckDuckGo HTML endpoint via POST. Returns `None` on network/parse trouble.
async fn ddg_html(client: &reqwest::Client, query: &str, num: usize) -> Option<String> {
    let q: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let html = client
        .post("https://html.duckduckgo.com/html/")
        .header("User-Agent", UA)
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("q={q}"))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;

    let link_re = RegexBuilder::new(r#"class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
        .dot_matches_new_line(true)
        .build()
        .ok()?;
    let snip_re = RegexBuilder::new(r#"class="result__snippet"[^>]*>(.*?)</a>"#)
        .dot_matches_new_line(true)
        .build()
        .ok()?;
    let snippets: Vec<String> = snip_re
        .captures_iter(&html)
        .map(|c| clean_html(&c[1]))
        .collect();

    let mut out: Vec<String> = Vec::new();
    for (i, cap) in link_re.captures_iter(&html).take(num).enumerate() {
        let url = decode_ddg_url(&cap[1]);
        let title = clean_html(&cap[2]);
        let snippet: String = snippets
            .get(i)
            .cloned()
            .unwrap_or_default()
            .chars()
            .take(600)
            .collect();
        out.push(format!("{title}\n{url}\n{snippet}"));
    }
    Some(out.join("\n\n---\n\n"))
}

/// DuckDuckGo Instant Answer API (stable JSON, no key) — good for definitions and facts.
async fn ddg_instant(client: &reqwest::Client, query: &str, num: usize) -> Option<String> {
    let q: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let url = format!("https://api.duckduckgo.com/?q={q}&format=json&no_html=1&no_redirect=1");
    let v: Value = client
        .get(&url)
        .header("User-Agent", UA)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    let mut out: Vec<String> = Vec::new();
    let abstract_text = v["AbstractText"].as_str().unwrap_or("");
    if !abstract_text.is_empty() {
        let url = v["AbstractURL"].as_str().unwrap_or("");
        out.push(format!(
            "{}\n{url}\n{abstract_text}",
            v["Heading"].as_str().unwrap_or("")
        ));
    }
    if let Some(rt) = v["RelatedTopics"].as_array() {
        for t in rt.iter() {
            if out.len() >= num {
                break;
            }
            if let Some(text) = t["Text"].as_str() {
                let url = t["FirstURL"].as_str().unwrap_or("");
                out.push(format!("{url}\n{text}"));
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("\n\n---\n\n"))
    }
}

pub struct WebSearch;

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for current information. Uses Exa if EXA_API_KEY is set, otherwise a \
         free key-less DuckDuckGo search. Returns titles, URLs, and snippets."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "num_results": { "type": "integer", "minimum": 1, "maximum": 10 }
            },
            "required": ["query"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx) -> ToolOutput {
        let Some(query) = input["query"].as_str() else {
            return ToolOutput::error("web_search: missing `query`");
        };
        let num = input["num_results"].as_u64().unwrap_or(5).clamp(1, 10);

        // Exa when a key is configured; otherwise the free DuckDuckGo fallback.
        if let Ok(key) = std::env::var("EXA_API_KEY") {
            let resp = reqwest::Client::new()
                .post("https://api.exa.ai/search")
                .header("x-api-key", key)
                .json(&exa_body(query, num))
                .send()
                .await;
            match resp.and_then(|r| r.error_for_status()) {
                Ok(r) => match r.json::<Value>().await {
                    Ok(v) => return ToolOutput::ok(format_results(&v)),
                    Err(e) => return ToolOutput::error(format!("web_search: bad response: {e}")),
                },
                Err(e) => return ToolOutput::error(format!("web_search (exa): {e}")),
            }
        }

        match ddg_search(query, num as usize).await {
            Ok(text) => ToolOutput::ok(text),
            Err(e) => ToolOutput::error(format!("web_search (duckduckgo): {e}")),
        }
    }
}

pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return its text content (truncated)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "url": { "type": "string" } },
            "required": ["url"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx) -> ToolOutput {
        let Some(url) = input["url"].as_str() else {
            return ToolOutput::error("web_fetch: missing `url`");
        };
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        let resp = client.get(url).header("User-Agent", UA).send().await;
        let resp = match resp.and_then(|r| r.error_for_status()) {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("web_fetch: {e}")),
        };
        match resp.text().await {
            Ok(body) => {
                let text: String = body.chars().take(8000).collect();
                ToolOutput::ok(text)
            }
            Err(e) => ToolOutput::error(format!("web_fetch: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exa_body_shape() {
        let b = exa_body("rust async", 3);
        assert_eq!(b["query"], "rust async");
        assert_eq!(b["numResults"], 3);
        assert_eq!(b["contents"]["text"]["maxCharacters"], 2000);
    }

    #[test]
    fn formats_results_and_empty() {
        let v = json!({ "results": [
            { "title": "T", "url": "http://x", "text": "some body text" }
        ]});
        let out = format_results(&v);
        assert!(out.contains("T"));
        assert!(out.contains("http://x"));
        assert!(out.contains("some body text"));

        assert_eq!(format_results(&json!({ "results": [] })), "no results");
    }

    #[test]
    fn clean_html_strips_tags_and_entities() {
        assert_eq!(
            clean_html("<b>Rust</b> &amp; <i>async</i>&#x27;s"),
            "Rust & async's"
        );
    }

    #[test]
    fn decode_ddg_redirect() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fbook%2F&rut=abc";
        assert_eq!(decode_ddg_url(href), "https://doc.rust-lang.org/book/");
        // A direct URL passes through.
        assert_eq!(decode_ddg_url("https://example.com"), "https://example.com");
    }
}
