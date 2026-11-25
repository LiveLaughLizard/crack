//! The `websearch` tool: search the web with no setup.
//!
//! Backends, in the order `auto` tries them:
//! 1. Z.ai's search (`web_search_prime`), when a Z.ai key is configured: GLM Coding
//!    Plan subscriptions include it.
//! 2. A SearXNG server, when `searxng_url` is set (self-hosted, no key).
//! 3. DuckDuckGo's lite results page: built in, no key.
//!
//! `web_search = "zai" | "searxng" | "duckduckgo" | "off"` picks one; `auto` falls back
//! to the next when one fails.
// Air-gapped builds keep the settings type but have no tool to use the rest.
#![cfg_attr(not(feature = "cloud"), allow(dead_code))]

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// `auto` (default), `zai`, `searxng`, `duckduckgo` or `off`.
    pub mode: String,
    pub searxng_url: Option<String>,
    pub zai_key: Option<String>,
}

impl Settings {
    pub fn from_config(config: &crate::config::Config) -> Settings {
        let zai = crate::providers::registry::provider_config(config, "zai");
        let zai_key = zai.api_key.filter(|k| !k.is_empty()).or_else(|| {
            zai.api_key_env
                .iter()
                .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
        });
        Settings {
            mode: config.web_search.clone().unwrap_or_else(|| "auto".into()),
            searxng_url: config.searxng_url.clone(),
            zai_key,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Searches; returns the hits and which backend answered.
pub async fn search(
    settings: &Settings,
    query: &str,
    max: usize,
    http: &reqwest::Client,
) -> anyhow::Result<(Vec<Hit>, &'static str)> {
    let mode = settings.mode.as_str();
    anyhow::ensure!(mode != "off", "web search is off (web_search = \"off\")");
    let mut errors = Vec::new();
    if matches!(mode, "auto" | "zai")
        && let Some(key) = &settings.zai_key
    {
        match zai(key, query, http).await {
            Ok(hits) => return Ok((cap(hits, max), "Z.ai")),
            Err(error) if mode == "auto" => errors.push(format!("Z.ai: {error:#}")),
            Err(error) => return Err(error),
        }
    } else if mode == "zai" {
        anyhow::bail!("web_search = \"zai\" needs a Z.ai key");
    }
    if matches!(mode, "auto" | "searxng")
        && let Some(url) = &settings.searxng_url
    {
        match searxng(url, query, http).await {
            Ok(hits) => return Ok((cap(hits, max), "SearXNG")),
            Err(error) if mode == "auto" => errors.push(format!("SearXNG: {error:#}")),
            Err(error) => return Err(error),
        }
    } else if mode == "searxng" {
        anyhow::bail!("web_search = \"searxng\" needs searxng_url");
    }
    if matches!(mode, "auto" | "duckduckgo") {
        match duckduckgo(query, http).await {
            Ok(hits) => return Ok((cap(hits, max), "DuckDuckGo")),
            Err(error) => errors.push(format!("DuckDuckGo: {error:#}")),
        }
    }
    anyhow::bail!("no search backend answered: {}", errors.join("; "))
}

fn cap(mut hits: Vec<Hit>, max: usize) -> Vec<Hit> {
    hits.truncate(max);
    hits
}

/// Results as the model sees them.
pub fn format(query: &str, hits: &[Hit], backend: &str) -> String {
    if hits.is_empty() {
        return format!("No results for \"{query}\" ({backend}).");
    }
    let mut out = format!("Results for \"{query}\" ({backend}):\n");
    for (i, hit) in hits.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n   {}\n", i + 1, hit.title, hit.url));
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
    }
    out.push_str("\nFetch a page with webfetch to read it in full.");
    out
}

// --------------------------------------------------------------------- Z.ai

/// One MCP connection to Z.ai's search server, reused across searches.
static ZAI: std::sync::Mutex<Option<(String, Arc<crate::mcp::McpHub>)>> =
    std::sync::Mutex::new(None);

async fn zai(key: &str, query: &str, http: &reqwest::Client) -> anyhow::Result<Vec<Hit>> {
    let hub = {
        let mut slot = ZAI.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some((k, hub)) if k == key => hub.clone(),
            _ => {
                let server = crate::mcp::McpServerConfig {
                    url: Some("https://api.z.ai/api/mcp/web_search_prime/mcp".into()),
                    headers: BTreeMap::from([(
                        "Authorization".to_owned(),
                        format!("Bearer {key}"),
                    )]),
                    ..Default::default()
                };
                let hub = crate::mcp::McpHub::new(
                    BTreeMap::from([("zai_search".to_owned(), server)]),
                    http.clone(),
                );
                *slot = Some((key.to_owned(), hub.clone()));
                hub
            }
        }
    };
    if hub.tools().is_empty() {
        hub.connect("zai_search").await;
    }
    let (text, is_error) = hub
        .call(
            "mcp__zai_search__web_search_prime",
            json!({"search_query": query}),
        )
        .await?;
    anyhow::ensure!(!is_error, "{}", text.chars().take(300).collect::<String>());
    parse_zai(&text)
}

/// The tool answers with a JSON array of results, sometimes as a JSON string of it.
fn parse_zai(text: &str) -> anyhow::Result<Vec<Hit>> {
    let mut value: Value = serde_json::from_str(text.trim())?;
    if let Value::String(inner) = &value {
        value = serde_json::from_str(inner)?;
    }
    let list = value.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "unexpected answer: {}",
            text.chars().take(200).collect::<String>()
        )
    })?;
    Ok(list
        .iter()
        .filter_map(|r| {
            Some(Hit {
                title: r["title"].as_str()?.trim().to_owned(),
                url: r["link"].as_str().or(r["url"].as_str())?.to_owned(),
                snippet: clean(r["content"].as_str().unwrap_or("")),
            })
        })
        .collect())
}

// ------------------------------------------------------------------ SearXNG

async fn searxng(base: &str, query: &str, http: &reqwest::Client) -> anyhow::Result<Vec<Hit>> {
    let url = reqwest::Url::parse_with_params(
        &format!("{}/search", base.trim_end_matches('/')),
        &[("q", query), ("format", "json")],
    )?;
    let response = http
        .get(url)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await?;
    anyhow::ensure!(
        response.status().is_success(),
        "HTTP {} (is format=json enabled on the server?)",
        response.status()
    );
    let json: Value = response.json().await?;
    Ok(json["results"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|r| {
                    Some(Hit {
                        title: r["title"].as_str()?.to_owned(),
                        url: r["url"].as_str()?.to_owned(),
                        snippet: clean(r["content"].as_str().unwrap_or("")),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

// --------------------------------------------------------------- DuckDuckGo

async fn duckduckgo(query: &str, http: &reqwest::Client) -> anyhow::Result<Vec<Hit>> {
    let response = http
        .post("https://lite.duckduckgo.com/lite/")
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) crack")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body(query))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await?;
    anyhow::ensure!(
        response.status().as_u16() == 200,
        "HTTP {}",
        response.status()
    );
    let html = response.text().await?;
    let hits = parse_duckduckgo(&html);
    anyhow::ensure!(
        !hits.is_empty() || html.contains("No results"),
        "the results page didn't parse (DuckDuckGo may be rate-limiting)"
    );
    Ok(hits)
}

/// `q=<query>`, form-encoded.
fn form_body(query: &str) -> String {
    reqwest::Url::parse_with_params("http://x/", &[("q", query)])
        .ok()
        .and_then(|u| u.query().map(str::to_owned))
        .unwrap_or_default()
}

fn parse_duckduckgo(html: &str) -> Vec<Hit> {
    let link = regex::Regex::new(r#"<a[^>]*href="([^"]+)"[^>]*class='result-link'[^>]*>(.*?)</a>"#)
        .expect("valid regex");
    let snippet =
        regex::Regex::new(r"(?s)<td class='result-snippet'>(.*?)</td>").expect("valid regex");
    let snippets: Vec<(usize, String)> = snippet
        .captures_iter(html)
        .map(|c| (c.get(0).map_or(0, |m| m.start()), clean(&c[1])))
        .collect();
    let links: Vec<_> = link.captures_iter(html).collect();
    let starts: Vec<usize> = links
        .iter()
        .map(|c| c.get(0).map_or(0, |m| m.start()))
        .collect();
    links
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            // Its snippet is the first one after it, before the next link.
            let after = c.get(0).map_or(0, |m| m.end());
            let before = starts.get(i + 1).copied().unwrap_or(usize::MAX);
            let snippet = snippets
                .iter()
                .find(|(at, _)| *at > after && *at < before)
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            let url = decode_entities(&c[1]);
            // Ads go through DuckDuckGo's own click tracker.
            if url.contains("duckduckgo.com/y.js") {
                return None;
            }
            Some(Hit {
                title: clean(&c[2]),
                url,
                snippet,
            })
        })
        .collect()
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

/// Plain text from a snippet: tags stripped, entities decoded, spaces collapsed.
fn clean(text: &str) -> String {
    let tags = regex::Regex::new(r"<[^>]+>").expect("valid regex");
    let text = decode_entities(&tags.replace_all(text, ""));
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duckduckgo_lite_and_drops_ads() {
        let html = r#"
            <a rel="nofollow" href="https://duckduckgo.com/y.js?ad=1" class='result-link'>Ad</a>
            <td class='result-snippet'>buy now</td>
            <a rel="nofollow" href="https://blog.rust-lang.org/2026/11/12/Rust-1.100.0/" class='result-link'>Announcing <b>Rust</b> 1.100.0</a>
            <td class='result-snippet'>
              The <b>Rust</b> team is happy &amp; proud.
            </td>"#;
        let hits = parse_duckduckgo(html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Announcing Rust 1.100.0");
        assert_eq!(
            hits[0].url,
            "https://blog.rust-lang.org/2026/11/12/Rust-1.100.0/"
        );
        assert_eq!(hits[0].snippet, "The Rust team is happy & proud.");
    }

    #[test]
    fn parses_zai_answers_even_double_encoded() {
        let inner = r#"[{"title":"Rust 1.100","link":"https://releases.rs/docs/1.100.0","content":"Released on: 12 November"}]"#;
        let double = serde_json::to_string(inner).unwrap();
        for text in [inner.to_owned(), double] {
            let hits = parse_zai(&text).unwrap();
            assert_eq!(hits[0].url, "https://releases.rs/docs/1.100.0");
            assert_eq!(hits[0].snippet, "Released on: 12 November");
        }
    }
}
