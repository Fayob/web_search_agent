use std::{sync::Arc, time::Duration};

use futures::future::join_all;
use serde_json::{Value, json};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::{
    config::Config, metrics::RunMetrics, structured::build_search_report, tools::{
        fetch_url_tool::FetchURLTool, get_crypto_price_tool::CryptoPriceTool,
        get_weather_tool::WeatherTool, search_web_tool::SearchWebTool, tool_registry::ToolRegistry,
    }, types::{AgentRunResult, SearchReport, TerminationReason, ToolCall, ToolResult}
};

pub struct RetryConfig {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay_ms: 1000,  // 1 second base
            max_delay_ms: 30_000, // 30 second ceiling
        }
    }
}

pub struct AgentConfig {
    pub max_iterations: u32,
    pub max_urls_fetched: usize,
    pub max_run_duration: Duration,
    pub token_budget_chars: usize,
    pub retry: RetryConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 10,
            max_urls_fetched: 5,
            max_run_duration: Duration::from_secs(180), // 3 minutes
            token_budget_chars: 128_000,
            retry: RetryConfig::default(),
        }
    }
}

pub struct SearchAgent {
    config: Arc<Config>,
    registry: ToolRegistry,
    agent_config: AgentConfig,
}

impl SearchAgent {
    pub fn new(config: Arc<Config>) -> Self {
        Self::with_agent_config(config, AgentConfig::default())
    }

    pub fn with_agent_config(config: Arc<Config>, agent_config: AgentConfig) -> Self {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(SearchWebTool::new(config.clone())));
        registry.register(Box::new(FetchURLTool::new(config.clone())));
        registry.register(Box::new(WeatherTool::new(config.clone())));
        registry.register(Box::new(CryptoPriceTool::new(config.clone())));

        Self {
            config,
            agent_config,
            registry,
        }
    }

    pub async fn run(&self, question: &str) -> anyhow::Result<AgentRunResult> {
        let run_start = Instant::now();

        info!(
            max_iterations = self.agent_config.max_iterations,
            max_urls = self.agent_config.max_urls_fetched,
            token_budget = self.agent_config.token_budget_chars,
            "agent run started"
        );

        let system_prompt = format!(
            "You are a research agent. Your job is to answer the user's \
             research question thoroughly using the available tools.\n\n\
             Strategy:\n\
             1. Start with search_web to find relevant sources\n\
             2. Use fetch_url to read the full content of the most promising URLs\n\
             3. Use get_weather or get_crypto_price if the question requires it\n\
             4. When you have enough information, produce a final answer\n\n\
             Rules:\n\
             - Never fetch more than 5 URLs per research session\n\
             - Always cite your sources with URLs\n\
             - If a tool fails, note it in limitations and continue\n\
             - Produce a final answer even with partial information"
        );

        let mut messages = vec![json!({ "role": "user", "content": question })];

        let mut tool_calls_log: Vec<ToolCall> = vec![];
        let mut recent_fingerprints: Vec<String> = vec![];
        let mut metrics = RunMetrics::default();
        let mut iterations = 0u32;

        loop {
            if iterations >= self.agent_config.max_iterations {
                warn!(
                    iterations = iterations,
                    "agent reached max iterations without completing"
                );
                return Ok(self.build_result(
                    iterations,
                    tool_calls_log,
                    None,
                    TerminationReason::MaxIterationsReached,
                    &metrics,
                    run_start,
                ));
            }
            iterations += 1;

            info!(
                iteration = iterations,
                message_count = messages.len(),
                estimated_tokens = metrics.estimated_tokens_used,
                "starting iteration"
            );

            let estimated_chars = estimate_context_size(&messages);
            metrics.estimated_tokens_used = estimated_chars / 4;

            if estimated_chars >= self.agent_config.token_budget_chars {
                warn!(
                    estimated_chars = estimated_chars,
                    budget_chars = self.agent_config.token_budget_chars,
                    message_count = messages.len(),
                    "context approaching token budget — pruning history"
                );
                prune_message_history(&mut messages);
            }

            let gemini_start = Instant::now();

            let api_response = match self.call_model_with_retry(&system_prompt, &messages).await {
                Ok((response, retries)) => {
                    let gemini_ms = gemini_start.elapsed().as_millis() as u64;
                    metrics.record_model_call(gemini_ms, retries);

                    debug!(
                        duration_ms = gemini_ms,
                        retries = retries,
                        "gemini call completed"
                    );
                    response
                }
                Err(e) => {
                    info!(error = %e, "gemini call failed after all retries");
                    return Ok(self.build_result(
                        iterations,
                        tool_calls_log,
                        None,
                        TerminationReason::FatalError(e.to_string()),
                        &metrics,
                        run_start,
                    ));
                }
            };

            let choice = &api_response["choices"][0];
            let finish_reason = choice["finish_reason"].as_str().unwrap_or("stop");
            let assistant_message = &choice["message"];

            info!(
                finish_reason = finish_reason,
                iteration = iterations,
                "model responded"
            );

            // model is done
            if finish_reason == "stop" {
                let final_content = assistant_message["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();

                let report = build_search_report(
                    question, 
                    &final_content, 
                    &tool_calls_log
                );

                info!(
                    iterations = iterations,
                    tool_calls = metrics.total_tool_calls,
                    success_rate = metrics.tool_success_rate(),
                    avg_gemini_ms = metrics.avg_model_latency_ms(),
                    estimated_tokens = metrics.estimated_tokens_used,
                    sources_count    = report.sources.len(),
                    confidence       = %report.confidence,
                    "agent run completed"
                );

                messages.push(assistant_message.clone());

                return Ok(self.build_result(
                    iterations,
                    tool_calls_log,
                    Some(report),
                    TerminationReason::Completed,
                    &metrics,
                    run_start,
                ));
            }

            // model wants tool calls
            let tool_calls = match assistant_message["tool_calls"].as_array() {
                Some(tc) => tc.clone(),
                None => {
                    info!(
                        finish_reason = finish_reason,
                        "unexpected model response — neither stop nor tool_calls"
                    );
                    return Ok(self.build_result(
                        iterations,
                        tool_calls_log,
                        None,
                        TerminationReason::FatalError(format!(
                            "unexpected finish_reason: {}",
                            finish_reason
                        )),
                        &metrics,
                        run_start,
                    ));
                }
            };
            messages.push(assistant_message.clone());

            info!(
                tool_call_count = tool_calls.len(),
                iteration = iterations,
                "executing tool calls concurrently"
            );

            // check all calls before executing any
            for tc in &tool_calls {
                let tool_name = tc["function"]["name"].as_str().unwrap_or("");
                let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                let fingerprint = format!("{}:{}", tool_name, args_str);

                let is_looping = recent_fingerprints
                    .iter()
                    .rev()
                    .take(3)
                    .any(|f| f == &fingerprint);

                if is_looping {
                    warn!(
                        tool = tool_name,
                        "loop detected — model repeating identical tool call"
                    );
                    return Ok(self.build_result(
                        iterations,
                        tool_calls_log,
                        None,
                        TerminationReason::LoopDetected,
                        &metrics,
                        run_start,
                    ));
                }

                recent_fingerprints.push(fingerprint);
            }

            // Concurrent tool execution
            let tool_futures: Vec<_> = tool_calls
                .iter()
                .map(|tc| {
                    let tool_id = tc["id"].as_str().unwrap_or("").to_string();
                    let tool_name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                    let args: Value =
                        serde_json::from_str(tc["function"]["arguments"].as_str().unwrap_or("{}"))
                            .unwrap_or_else(|_| json!({}));

                    let registry = &self.registry;
                    let agent_config = &self.agent_config;
                    let urls_fetched = metrics.urls_fetched;

                    async move {
                        if tool_name == "fetch_url" && urls_fetched >= agent_config.max_urls_fetched
                        {
                            warn!(
                                max_urls = agent_config.max_urls_fetched,
                                "URL fetch limit reached — skipping"
                            );
                            return (
                                tool_id,
                                tool_name,
                                args,
                                ToolResult::Error {
                                    category: "non_retryable".to_string(),
                                    message: format!(
                                        "URL fetch limit reached ({} max). \
                                         Synthesize from information gathered so far.",
                                        agent_config.max_urls_fetched
                                    ),
                                },
                                0u64,
                            );
                        }

                        let call_start = Instant::now();

                        let raw_result = registry.execute(&tool_name, &args).await;

                        let duration_ms = call_start.elapsed().as_millis() as u64;

                        let tool_result = match &raw_result {
                            Ok(data) => ToolResult::ok(data.clone()),
                            Err(e) => ToolResult::from_error(e),
                        };

                        (tool_id, tool_name, args, tool_result, duration_ms)
                    }
                })
                .collect();

            let tool_outcomes = join_all(tool_futures).await;

            for (tool_id, tool_name, args, tool_result, duration_ms) in tool_outcomes {
                if tool_name == "fetch_url" {
                    if matches!(&tool_result, ToolResult::Success { .. }) {
                        metrics.urls_fetched += 1;
                    }
                }

                match &tool_result {
                    ToolResult::Success { .. } => {
                        metrics.record_tool_success(duration_ms);
                        info!(
                            tool        = %tool_name,
                            duration_ms = duration_ms,
                            "tool call succeeded"
                        );
                    }

                    ToolResult::Error { category, message } => {
                        metrics.record_tool_failure(duration_ms);
                        warn!(
                            tool        = %tool_name,
                            duration_ms = duration_ms,
                            category    = %category,
                            message     = %message,
                            "tool call failed"
                        );
                    }
                }

                tool_calls_log.push(ToolCall {
                    tool_name: tool_name.clone(),
                    input: args,
                    output: tool_result.clone(),
                    duration_ms,
                });

                let result_content = serde_json::to_string(&tool_result).unwrap_or_else(|_| {
                    r#"{"status":"error","message":"serialization failed"}"#.to_string()
                });

                // println!("Got here: {:?}...", &result_content);

                messages.push(json!({
                    "role":         "tool",
                    "tool_call_id": tool_id,
                    "content":      result_content
                }));
            }
        }
    }

    async fn call_model_with_retry(
        &self,
        system: &str,
        messages: &[Value],
    ) -> anyhow::Result<(Value, u32)> {
        let mut attempt = 0u32;
        let mut last_err = String::new();

        loop {
            if attempt >= self.agent_config.retry.max_attempts {
                anyhow::bail!(
                    "gemini call failed after {} attempts. Last error: {}",
                    attempt,
                    last_err
                );
            }

            // Wait before retrying — not before the first attempt
            if attempt > 0 {
                let wait = std::time::Duration::from_secs(2_u64.pow(attempt as u32 - 1));

                warn!(
                    attempt  = attempt,
                    wait_ms  = wait.as_millis(),
                    last_err = %last_err,
                    "retrying Gemini call after backoff"
                );
                tokio::time::sleep(wait).await;
            }

            attempt += 1;

            let result = self.call_model_once(system, messages).await;

            match result {
                Ok(response) => {
                    if attempt > 1 {
                        info!(attempt = attempt, "model call succeeded after retry");
                    }
                    return Ok((response, attempt - 1));
                }

                Err(e) => {
                    let err_str = e.to_string();

                    let is_retryable = err_str.contains("429")
                        || err_str.contains("500")
                        || err_str.contains("502")
                        || err_str.contains("503")
                        || err_str.contains("504")
                        || err_str.contains("timed out")
                        || err_str.contains("connection");

                    if !is_retryable {
                        info!(
                            error   = %err_str,
                            attempt = attempt,
                            "non-retryable Gemini error — failing immediately"
                        );
                        return Err(e);
                    }

                    warn!(
                        error   = %err_str,
                        attempt = attempt,
                        "retryable Gemini error"
                    );
                    last_err = err_str;
                }
            }
        }
    }

    async fn call_model_once(&self, system: &str, messages: &[Value]) -> anyhow::Result<Value> {
        let mut full_messages = vec![json!({ "role": "system", "content": system })];
        full_messages.extend_from_slice(messages);

        let tools = self.registry.descriptions();

        let request_body = json!({
            "model":       "gemini-2.5-flash",
            "temperature": 0.1,
            "tool_choice": "auto",
            "tools":       tools,
            "messages":    full_messages
        });

        debug!(
            message_count = full_messages.len(),
            tool_count = tools.len(),
            "sending request to Gemini"
        );

        let response = self
            .config
            .http_client
            .post("https://generativelanguage.googleapis.com/v1beta/openai/chat/completions")
            .header(
                "Authorization",
                format!("Bearer {}", self.config.gemini_api_key),
            )
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("HTTP send failed: {}", e))?;

        let status = response.status();

        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());

            println!("The error: {} — {}", status.as_u16(), error_body);
            anyhow::bail!("{} — {}", status.as_u16(), error_body);
        }

        let data: Value = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("failed to parse model response: {}", e))?;

        Ok(data)
    }

    fn build_result(
        &self,
        iterations: u32,
        tool_calls: Vec<ToolCall>,
        report: Option<SearchReport>,
        termination_reason: TerminationReason,
        metrics: &RunMetrics,
        run_start: Instant,
    ) -> AgentRunResult {
        let total_ms = run_start.elapsed().as_millis() as u64;

        info!(
            total_ms              = total_ms,
            iterations            = iterations,
            total_tool_calls      = metrics.total_tool_calls,
            successful_tool_calls = metrics.successful_tool_calls,
            failed_tool_calls     = metrics.failed_tool_calls,
            model_calls          = metrics.model_calls,
            model_retries        = metrics.model_retries,
            avg_model_ms         = metrics.avg_model_latency_ms(),
            tool_success_rate     = metrics.tool_success_rate(),
            estimated_tokens      = metrics.estimated_tokens_used,
            urls_fetched          = metrics.urls_fetched,
            termination_reason    = ?termination_reason,
            "agent run finished"
        );

        AgentRunResult {
            iterations,
            tool_calls,
            report,
            termination_reason,
        }
    }
}

fn prune_message_history(messages: &mut Vec<Value>) {
    if messages.len() <= 10 {
        return;
    }

    if messages.len() > 2 {
        messages.remove(1);
        if messages.len() > 2 {
            messages.remove(1);
        }
    }

    warn!(
        remaining_messages = messages.len(),
        "pruned oldest message exchange from history"
    );
}

fn estimate_context_size(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|m| {
            m["content"].as_str().map(|s| s.len()).unwrap_or(0)
                + m["tool_calls"]
                    .as_array()
                    .map(|tc| {
                        tc.iter()
                            .map(|t| {
                                t["function"]["arguments"]
                                    .as_str()
                                    .map(|s| s.len())
                                    .unwrap_or(0)
                            })
                            .sum::<usize>()
                    })
                    .unwrap_or(0)
        })
        .sum()
}

pub fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS        // 429 rate limit
        | reqwest::StatusCode::INTERNAL_SERVER_ERROR  // 500
        | reqwest::StatusCode::BAD_GATEWAY            // 502
        | reqwest::StatusCode::SERVICE_UNAVAILABLE    // 503
        | reqwest::StatusCode::GATEWAY_TIMEOUT // 504
    )
}
