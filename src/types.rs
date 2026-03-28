use std::fmt::Display;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum ToolError {
    Retryable(String),
    NonRetryable(String),
    Degraded { url: String, reason: String },
}

impl Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ToolError::Retryable(msg) => write!(f, "retryable: {}", msg),
            ToolError::NonRetryable(msg) => write!(f, "non_retryable: {}", msg),
            ToolError::Degraded { url, reason } => write!(f, "degraded({url}): {reason}"),
        }
    }
}

impl std::error::Error for ToolError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolResult {
    Success { data: serde_json::Value },
    Error { category: String, message: String },
}

impl ToolResult {
    pub fn ok(data: serde_json::Value) -> Self {
        ToolResult::Success { data }
    }

    pub fn from_error(e: &ToolError) -> Self {
        let (category, message) = match e {
            ToolError::Retryable(msg) => ("retryable", msg.clone()),
            ToolError::NonRetryable(msg) => ("non_retryable", msg.clone()),
            ToolError::Degraded { url, reason } => ("degraded", format!("{url}: {reason}")),
        };

        ToolResult::Error {
            category: category.into(),
            message,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool_name: String,
    pub input: serde_json::Value,
    pub output: ToolResult,
    pub duration_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentRunResult {
    pub iterations: u32,
    pub tool_calls: Vec<ToolCall>,
    pub report: Option<SearchReport>,
    pub termination_reason: TerminationReason,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    Completed,
    MaxIterationsReached,
    LoopDetected,
    FatalError(String),
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchReport {
    pub question: String,
    pub answer: String,
    pub sources: Vec<Source>,
    pub confidence: String,
    pub key_findings: Vec<String>,
    pub search_queries: Vec<String>,
    pub limitations: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct Source {
    pub title: String,
    pub url: String,
    pub snippet: String,
}
