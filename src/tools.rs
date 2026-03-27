use std::sync::Arc;

use scraper::{Html, Selector};
use serde_json::{Value, json};

use crate::{config::Config, types::ToolError};

pub fn search_web_description() -> Value {
    json!({
        "name": "search_web",
        "description": "Search the web using Brave Search API.
                    Use this to find relevant URLs and snippets for a research question. 
                    Pass a concise search query of 3-7 words. Returns titles, URLs, and 
                    snippets but NOT full page content. Use fetch_url separately to read 
                    full content from promising URLs.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query. Keep it concise, 3-7 words.
                                Do not pass full sentences or URLs."
                },
                "count": {
                    "type": "integer",
                    "description": "Number of results to return. Default 5, maximum 10."
                }
            },
            "required": ["query"]
        }
    })
}

pub async fn search_web(
    config: Arc<Config>,
    query: &str,
    count: usize,
) -> Result<Value, ToolError> {
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencoding::encode(query),
        count.min(10)
    );

    let response = config
        .http_client
        .get(&url)
        .header("X-Subscription-Token", &config.brave_api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                ToolError::Retryable(format!("search timed out: {}", e))
            } else if e.is_connect() {
                ToolError::Retryable(format!("connection failed: {}", e))
            } else {
                ToolError::NonRetryable(format!("request failed: {}", e))
            }
        })?;

    let status = response.status();

    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ToolError::NonRetryable(
            "Brave API key is invalid or missing".to_string(),
        ));
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(ToolError::Retryable(
            "Brave API rate limit hit — back off and retry".to_string(),
        ));
    }

    if !status.is_success() {
        return Err(ToolError::NonRetryable(format!(
            "Brave API returned status: {}",
            status
        )));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| ToolError::NonRetryable(
             format!("failed to parse Brave response as JSON: {}", e)
        ))?;

    let results = body
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(|r| r.as_array())
        .ok_or_else(|| ToolError::Degraded { 
            url: format!("https://api.search.brave.com/?q={}", query),
            reason: "no web results in response".to_string(),
        })?;

    let shaped: Vec<Value> = results
        .iter()
        .take(count)
        .map(|r| json!({
            "title": r.get("title").and_then(|v| v.as_str()).unwrap_or(""),
            "url": r.get("url").and_then(|v| v.as_str()).unwrap_or(""),
            "snippet": r.get("description").and_then(|v| v.as_str()).unwrap_or(""),
        }))
        .collect();

    Ok(json!({
        "query": query,
        "result_count": shaped.len(),
        "results": shaped
    }))

}

pub fn fetch_url_description() -> Value {
    json!({
        "name": "fetch_url",
        "description": "Fetch and read the full text content of a web page. 
                    Use this after search_web to read the actual content of 
                    promising URLs. Strips HTML tags and returns clean text. 
                    Content is truncated to ~3000 characters. Do not use this 
                    for search — use search_web first to find relevant URLs.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The full URL to fetch including https://"
                }
            },
            "required": ["url"]
        }
    })
}

pub async fn fetch_url(
    config: Arc<Config>,
    url: &str
) -> Result<Value, ToolError> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ToolError::NonRetryable(
            format!("invalid URL — must start with http:// or https://: {}", url)
        ));
    }

    let response = config
        .http_client
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                ToolError::Degraded { 
                    url: url.to_string(), 
                    reason: format!("timed out after 15s: {}", e),
                }
            } else {
                ToolError::Degraded {
                    url: url.to_string(),
                    reason: format!("connection failed: {}", e),
                }
            }
        })?;

        let status = response.status();

        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ToolError::Degraded {
                url: url.to_string(),
                reason: format!("access denied ({})", status),
            });
        }

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ToolError::Degraded {
                url: url.to_string(),
                reason: "page not found (404)".to_string(),
            });
        }

        if !status.is_success() {
            return Err(ToolError::Degraded {
                url: url.to_string(),
                reason: format!("HTTP error: {}", status),
            });
        }

        let html = response
            .text()
            .await
            .map_err(|e| ToolError::Degraded { 
                url: url.to_string(),
                reason: format!("failed to read response body: {}", e),
            })?;

        let clean_text = extract_clean_text(&html);

        if clean_text.trim().is_empty() {
            return Err(ToolError::Degraded {
                url: url.to_string(),
                reason: "page contained no extractable text content".to_string(),
            });
        }

        let truncated = if clean_text.len() > 3000 {
            format!("{}... [truncated]", &clean_text[..3000])
        } else {
            clean_text
        };

        Ok(json!({
            "url": url,
            "content": truncated,
            "content_length": truncated.len()
        }))
}

fn extract_clean_text(html: &str) -> String {
    let document = Html::parse_document(html);

    let selector = Selector::parse(
        "p, h1, h2, h3, h4, h5, h6, article, main, li"
    ).unwrap();

    let mut text_parts: Vec<String> = document
        .select(&selector)
        .map(|element| element.text().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.trim().is_empty())
        .collect();

    text_parts.dedup();

    text_parts.join("\n")
}
