use std::sync::Arc;

use async_trait::async_trait;
use scraper::{Html, Selector};
use serde_json::{Value, json};

use crate::{config::Config, tools::Tool, types::ToolError};

pub struct FetchURLTool {
    config: Arc<Config>,
}

impl FetchURLTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for FetchURLTool {
    fn name(&self) -> &str {
        "fetch_url"
    }

    fn description(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
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
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let url = args["url"].as_str().ok_or_else(|| {
            ToolError::NonRetryable("missing required parameter: url".to_string())
        })?;

        fetch_url(&self.config, url).await
    }
}

pub async fn fetch_url(config: &Arc<Config>, url: &str) -> Result<Value, ToolError> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ToolError::NonRetryable(format!(
            "invalid URL — must start with http:// or https://: {}",
            url
        )));
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

    let html = response.text().await.map_err(|e| ToolError::Degraded {
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

    let selector = Selector::parse("p, h1, h2, h3, h4, h5, h6, article, main, li").unwrap();

    let mut text_parts: Vec<String> = document
        .select(&selector)
        .map(|element| element.text().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.trim().is_empty())
        .collect();

    text_parts.dedup();

    text_parts.join("\n")
}
