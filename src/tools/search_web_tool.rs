use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{config::Config, tools::Tool, types::ToolError};

pub struct SearchWebTool {
    config: Arc<Config>,
}

impl SearchWebTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for SearchWebTool {
    fn name(&self) -> &str {
        "search_web"
    }

    fn description(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name(),
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
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let query = args["query"].as_str().ok_or_else(|| {
            ToolError::NonRetryable("missing required parameter: query".to_string())
        })?;
        let count = args["count"].as_u64().unwrap_or(5) as usize;

        if self.config.tavily_api_key.is_some() {
            tavily_search(&self.config, query, count).await
        } else {
            search_web(&self.config, query, count).await
        }
    }
}

pub async fn search_web(
    config: &Arc<Config>,
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

    let body: Value = response.json().await.map_err(|e| {
        ToolError::NonRetryable(format!("failed to parse Brave response as JSON: {}", e))
    })?;

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
        .map(|r| {
            json!({
                "title": r.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "url": r.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                "snippet": r.get("description").and_then(|v| v.as_str()).unwrap_or(""),
            })
        })
        .collect();

    Ok(json!({
        "query": query,
        "result_count": shaped.len(),
        "results": shaped
    }))
}

pub async fn tavily_search(
    config: &Arc<Config>,
    query: &str,
    count: usize,
) -> Result<Value, ToolError> {
    let api_key = config.tavily_api_key.as_deref().ok_or_else(|| {
        ToolError::NonRetryable("TAVILY_API_KEY is not set".to_string())
    })?;

    let body = json!({
        "api_key": api_key,
        "query": query,
        "max_results": count.min(10)
    });

    let response = config
        .http_client
        .post("https://api.tavily.com/search")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                ToolError::Retryable(format!("Tavily search timed out: {}", e))
            } else if e.is_connect() {
                ToolError::Retryable(format!("Tavily connection failed: {}", e))
            } else {
                ToolError::NonRetryable(format!("Tavily request failed: {}", e))
            }
        })?;

    let status = response.status();

    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ToolError::NonRetryable(
            "Tavily API key is invalid or missing".to_string(),
        ));
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(ToolError::Retryable(
            "Tavily API rate limit hit — back off and retry".to_string(),
        ));
    }

    if !status.is_success() {
        return Err(ToolError::NonRetryable(format!(
            "Tavily API returned status: {}",
            status
        )));
    }

    let body: Value = response.json().await.map_err(|e| {
        ToolError::NonRetryable(format!("failed to parse Tavily response as JSON: {}", e))
    })?;

    let results = body
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or_else(|| ToolError::Degraded {
            url: format!("https://api.tavily.com/search?query={}", query),
            reason: "no results in Tavily response".to_string(),
        })?;

    let shaped: Vec<Value> = results
        .iter()
        .take(count)
        .map(|r| {
            json!({
                "title": r.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "url": r.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                "snippet": r.get("content").and_then(|v| v.as_str()).unwrap_or(""),
            })
        })
        .collect();

    Ok(json!({
        "query": query,
        "result_count": shaped.len(),
        "results": shaped
    }))
}
