//! Narrow Streamable HTTP MCP client for the agent's search_web tool.
//! Uses the same reqwest transport as the other tools; no background tasks.
use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::{Client, Response, header::HeaderValue};
use serde_json::{Value, json};
use tokio::sync::OnceCell;

use crate::{
    config::{MODEL_NAME, USER_AGENT},
    types::ToolError,
};

pub(crate) const ENDPOINT: &str = "https://search.parallel.ai/mcp";
const PROTOCOL: &str = "2025-03-26";
const MAX_RESPONSE_BYTES: usize = 1_048_576;

struct Session {
    id: Option<HeaderValue>,
}

pub(crate) struct ParallelSearchClient {
    client: Client,
    endpoint: String,
    session: OnceCell<Session>,
    next_id: AtomicU64,
}

impl ParallelSearchClient {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            endpoint: ENDPOINT.to_string(),
            session: OnceCell::new(),
            next_id: AtomicU64::new(1),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_endpoint(client: Client, endpoint: String) -> Self {
        Self {
            endpoint,
            ..Self::new(client)
        }
    }

    async fn post(
        &self,
        payload: &Value,
        session: Option<&Session>,
    ) -> Result<Response, ToolError> {
        let mut request = self
            .client
            .post(&self.endpoint)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL)
            .json(payload);
        if let Some(id) = session.and_then(|s| s.id.as_ref()) {
            request = request.header("Mcp-Session-Id", id);
        }
        let response = request.send().await.map_err(network_error)?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = response
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(invalid("error response exceeds 1 MiB"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let detail = String::from_utf8_lossy(&bytes);
        let message =
            format!("Parallel MCP returned HTTP {status}: {detail}; Retry-After: {retry_after}");
        if status.as_u16() == 429 || status.as_u16() == 408 || status.is_server_error() {
            Err(ToolError::Retryable(message))
        } else {
            Err(ToolError::NonRetryable(message))
        }
    }

    async fn rpc(
        &self,
        method: &str,
        params: Value,
        session: Option<&Session>,
    ) -> Result<Value, ToolError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let response = self
            .post(
                &json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}),
                session,
            )
            .await?;
        read_rpc(response, id).await
    }

    async fn initialize(&self) -> Result<Session, ToolError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let response = self.post(&json!({
            "jsonrpc":"2.0", "id":id, "method":"initialize",
            "params": {"protocolVersion":PROTOCOL, "capabilities":{},
                "clientInfo":{"name":"web_search_agent", "version":env!("CARGO_PKG_VERSION")}}
        }), None).await?;
        let session = Session {
            id: response.headers().get("Mcp-Session-Id").cloned(),
        };
        let result = read_rpc(response, id).await?;
        if result["protocolVersion"] != PROTOCOL {
            return Err(invalid("unsupported negotiated protocol version"));
        }
        self.post(
            &json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
            Some(&session),
        )
        .await?;
        let tools = self.rpc("tools/list", json!({}), Some(&session)).await?;
        if !tools["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t["name"] == "web_search"))
        {
            return Err(invalid("server does not advertise web_search"));
        }
        Ok(session)
    }

    pub(crate) async fn search(
        &self,
        query: &str,
        count: usize,
        conversation_id: &str,
    ) -> Result<Value, ToolError> {
        let session = self.session.get_or_try_init(|| self.initialize()).await?;
        let result = self
            .rpc(
                "tools/call",
                json!({
                    "name":"web_search", "arguments": {
                        "objective": query, "search_queries":[query],
                        "session_id":conversation_id, "model_name":MODEL_NAME
                    }
                }),
                Some(session),
            )
            .await?;
        if result["isError"].as_bool() == Some(true) {
            let message = result["content"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| "tool call failed".to_string());
            let lower = message.to_lowercase();
            return Err(if lower.contains("rate limit") || lower.contains("quota") {
                ToolError::Retryable(format!("Parallel MCP: {message}"))
            } else {
                ToolError::NonRetryable(format!("Parallel MCP: {message}"))
            });
        }
        // MCP structuredContent is preferred. Older servers use a JSON text block.
        let data = if let Some(data) = result.get("structuredContent") {
            data.clone()
        } else {
            result["content"]
                .as_array()
                .and_then(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .find_map(|text| serde_json::from_str::<Value>(text).ok())
                })
                .ok_or_else(|| invalid("missing structured search result"))?
        };
        let rows = data["results"]
            .as_array()
            .ok_or_else(|| invalid("missing results array"))?;
        let mut shaped = Vec::new();
        for row in rows.iter().take(count) {
            let url = row["url"]
                .as_str()
                .ok_or_else(|| invalid("result has no URL"))?;
            let parsed = reqwest::Url::parse(url).map_err(|_| invalid("result has invalid URL"))?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return Err(invalid("result URL must use HTTP or HTTPS"));
            }
            let excerpts = row["excerpts"]
                .as_array()
                .ok_or_else(|| invalid("result has no excerpts"))?;
            let excerpts = excerpts
                .iter()
                .map(|part| part.as_str().ok_or_else(|| invalid("invalid excerpt")))
                .collect::<Result<Vec<_>, _>>()?;
            shaped.push(json!({"title":row["title"].as_str().unwrap_or(""), "url":url, "snippet":excerpts.join("\n")}));
        }
        Ok(json!({"query":query, "result_count":shaped.len(), "results":shaped}))
    }
}

fn invalid(message: &str) -> ToolError {
    ToolError::NonRetryable(format!("Parallel MCP: {message}"))
}

fn network_error(error: reqwest::Error) -> ToolError {
    if error.is_timeout() || error.is_connect() || error.is_body() {
        ToolError::Retryable(format!("Parallel MCP request failed: {error}"))
    } else {
        ToolError::NonRetryable(format!("Parallel MCP request failed: {error}"))
    }
}

fn rpc_result(value: Value, id: u64) -> Result<Value, ToolError> {
    if value["jsonrpc"] != "2.0" || value["id"].as_u64() != Some(id) {
        return Err(invalid("invalid JSON-RPC response identity"));
    }
    if let Some(error) = value.get("error") {
        return Err(invalid(&format!("JSON-RPC error: {error}")));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| invalid("missing JSON-RPC result"))
}

async fn read_rpc(mut response: Response, id: u64) -> Result<Value, ToolError> {
    let is_sse = response
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next() == Some("text/event-stream"));
    let mut bytes = Vec::new();
    let mut received = 0;
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        received += chunk.len();
        if received > MAX_RESPONSE_BYTES {
            return Err(invalid("response exceeds 1 MiB"));
        }
        bytes.extend_from_slice(&chunk);
        if is_sse {
            // Read complete SSE events and return as soon as this request is answered,
            // even if the server keeps its HTTP response open for notifications.
            loop {
                let boundary = bytes
                    .windows(2)
                    .position(|w| w == b"\n\n")
                    .map(|i| (i, 2))
                    .into_iter()
                    .chain(
                        bytes
                            .windows(4)
                            .position(|w| w == b"\r\n\r\n")
                            .map(|i| (i, 4)),
                    )
                    .min_by_key(|(position, _)| *position);
                let Some((end, separator)) = boundary else {
                    break;
                };
                let event = String::from_utf8(bytes.drain(..end + separator).collect())
                    .map_err(|_| invalid("invalid UTF-8 event"))?;
                let data = event
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("data:")
                            .map(|data| data.strip_prefix(' ').unwrap_or(data))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() {
                    continue;
                }
                let value: Value =
                    serde_json::from_str(&data).map_err(|_| invalid("invalid JSON event"))?;
                if value["id"].as_u64() == Some(id) {
                    return rpc_result(value, id);
                }
            }
        }
    }
    if is_sse {
        return Err(invalid("stream ended before JSON-RPC response"));
    }
    let value = serde_json::from_slice(&bytes).map_err(|_| invalid("invalid JSON response"))?;
    rpc_result(value, id)
}
