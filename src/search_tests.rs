use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

use crate::{
    agent::SearchAgent,
    config::{Config, MODEL_NAME, SearchProvider, USER_AGENT},
    parallel_search::{ENDPOINT, ParallelSearchClient},
    structured::build_search_report,
    tools::{Tool, search_web_tool::SearchWebTool, tool_registry::ToolRegistry},
    types::{TerminationReason, ToolCall, ToolError, ToolResult},
};

#[derive(Clone, Debug)]
struct Request {
    line: String,
    headers: String,
    body: Value,
}

struct Reply {
    status: u16,
    body: String,
    sse: bool,
    hold_open: bool,
}

impl Reply {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            body: body.to_string(),
            sse: false,
            hold_open: false,
        }
    }
}

struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        Self::start_gated(handler, None).await
    }

    async fn start_gated(
        handler: impl Fn(&Request) -> Reply + Send + Sync + 'static,
        model_gate: Option<Arc<tokio::sync::Barrier>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                let handler = handler.clone();
                let model_gate = model_gate.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let (headers, body_start) = loop {
                        let mut chunk = [0u8; 4096];
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                        if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            break (String::from_utf8(bytes[..i].to_vec()).unwrap(), i + 4);
                        }
                    };
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    while bytes.len() < body_start + length {
                        let mut chunk = [0u8; 4096];
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                    }
                    let body = serde_json::from_slice(&bytes[body_start..body_start + length])
                        .unwrap_or(Value::Null);
                    let request = Request {
                        line: headers.lines().next().unwrap().to_string(),
                        headers,
                        body,
                    };
                    captured.lock().unwrap().push(request.clone());
                    let reply = handler(&request);
                    // Synchronize each model round so both runs' first searches
                    // finish before either run can make its second search.
                    if request.line.contains("/model ") {
                        if let Some(gate) = model_gate {
                            gate.wait().await;
                        }
                    }
                    let kind = if reply.sse {
                        "text/event-stream"
                    } else {
                        "application/json"
                    };
                    let framing = if reply.hold_open {
                        "Transfer-Encoding: chunked\r\n".to_string()
                    } else {
                        format!(
                            "Content-Length: {}\r\nConnection: close\r\n",
                            reply.body.len()
                        )
                    };
                    let head = format!(
                        "HTTP/1.1 {} Response\r\nContent-Type: {kind}\r\nMcp-Session-Id: fixture-session\r\n{framing}\r\n",
                        reply.status
                    );
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let body = if reply.hold_open {
                        format!("{:x}\r\n{}\r\n", reply.body.len(), reply.body)
                    } else {
                        reply.body
                    };
                    let _ = socket.write_all(body.as_bytes()).await;
                    if reply.hold_open {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                });
            }
        });
        Self {
            endpoint,
            requests,
            task,
        }
    }

    fn snapshot(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

fn normal_reply(request: &Request, result: Value) -> Reply {
    let result = match request.body["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
        }
        Some("notifications/initialized") => {
            return Reply {
                status: 202,
                body: String::new(),
                sse: false,
                hold_open: false,
            };
        }
        Some("tools/list") => json!({"tools":[{"name":"web_search"}]}),
        _ => result,
    };
    Reply::json(json!({"jsonrpc":"2.0","id":request.body["id"],"result":result}))
}

fn results() -> Value {
    json!({"structuredContent":{"results":[
        {"title":"Tokio","url":"https://tokio.rs/tokio/tutorial?x=1&y=2","excerpts":["Rust async runtime.","Concurrent I/O 🦀"]},
        {"title":null,"url":"https://rust-lang.org/","excerpts":["Rust language."]}
    ]}})
}

fn config(selection: Option<&str>, key: Option<&str>) -> Config {
    Config::from_lookup(|name| match name {
        "SEARCH_PROVIDER" => selection.map(str::to_string),
        "BRAVE_API_KEY" => key.map(str::to_string),
        "GEMINI_API_KEY" | "OPENWEATHER_API_KEY" => Some("fixture-only".to_string()),
        _ => None,
    })
    .unwrap()
}

fn local_config(server: &Server, timeout: Duration) -> Config {
    local_config_with_selection(server, timeout, None, None)
}

fn local_config_with_selection(
    server: &Server,
    timeout: Duration,
    selection: Option<&str>,
    key: Option<&str>,
) -> Config {
    let mut config = config(selection, key);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(timeout)
        .user_agent(USER_AGENT)
        .build()
        .unwrap();
    config.parallel_search =
        ParallelSearchClient::with_endpoint(client.clone(), server.endpoint.clone());
    config.http_client = client;
    config
}

#[test]
fn provider_resolution_preserves_keys_and_explicit_choices() {
    assert_eq!(config(None, None).search_provider, SearchProvider::Parallel);
    assert_eq!(
        config(None, Some("  ")).search_provider,
        SearchProvider::Parallel
    );
    assert_eq!(
        config(None, Some("saved-brave-key")).search_provider,
        SearchProvider::Brave
    );
    assert_eq!(
        config(Some("parallel"), Some("saved-brave-key")).search_provider,
        SearchProvider::Parallel
    );
    assert_eq!(
        config(Some("brave"), Some("saved-brave-key")).search_provider,
        SearchProvider::Brave
    );
    assert_eq!(
        config(Some("none"), Some("saved-brave-key")).search_provider,
        SearchProvider::None
    );
    for (selection, key) in [("brave", ""), ("invalid", "saved-key"), ("", "")] {
        assert!(
            Config::from_lookup(|name| match name {
                "SEARCH_PROVIDER" => Some(selection.into()),
                "BRAVE_API_KEY" => Some(key.into()),
                _ => Some("fixture-only".into()),
            })
            .is_err()
        );
    }
    assert_eq!(ENDPOINT, "https://search.parallel.ai/mcp");
}

#[tokio::test]
async fn registry_search_preserves_contract_and_results_reach_report_consumer() {
    let server = Server::start(|request| normal_reply(request, results())).await;
    let registry =
        ToolRegistry::from_config(local_config(&server, Duration::from_secs(1)).into_arc());
    let args = json!({"query":"Rust Tokio async", "count":1});
    let output = registry.execute("search_web", &args).await.unwrap();
    assert_eq!(output["result_count"], 1);
    assert_eq!(
        output["results"][0]["url"],
        "https://tokio.rs/tokio/tutorial?x=1&y=2"
    );
    assert_eq!(
        output["results"][0]["snippet"],
        "Rust async runtime.\nConcurrent I/O 🦀"
    );
    // This is the same ToolResult envelope appended to model history by SearchAgent.
    let tool_result = ToolResult::ok(output);
    let serialized = serde_json::to_value(&tool_result).unwrap();
    assert_eq!(serialized["data"]["results"][0]["title"], "Tokio");
    let calls = [ToolCall {
        tool_name: "search_web".into(),
        input: args.clone(),
        output: tool_result,
        duration_ms: 0,
    }];
    assert_eq!(
        build_search_report("question", "answer", &calls).search_queries,
        vec!["Rust Tokio async"]
    );
    let requests = server.snapshot();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests
            .iter()
            .map(|r| r.body["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call"
        ]
    );
    for (i, r) in requests.iter().enumerate() {
        assert_eq!(r.line, "POST /mcp HTTP/1.1");
        let headers = r.headers.to_lowercase();
        assert!(headers.contains("user-agent: websearchagent/1.0"));
        assert!(headers.contains("accept: application/json, text/event-stream"));
        assert!(headers.contains("mcp-protocol-version: 2025-03-26"));
        assert!(!headers.contains("authorization:"));
        assert!(!headers.contains("x-api-key:"));
        if i > 0 {
            assert!(headers.contains("mcp-session-id: fixture-session"));
        }
    }
    let arguments = &requests[3].body["params"]["arguments"];
    assert_eq!(arguments["objective"], args["query"]);
    assert_eq!(arguments["search_queries"], json!(["Rust Tokio async"]));
    assert_eq!(arguments["model_name"], MODEL_NAME);
    assert!(uuid::Uuid::parse_str(arguments["session_id"].as_str().unwrap()).is_ok());
    registry
        .execute("search_web", &json!({"query":"second query"}))
        .await
        .unwrap();
    let requests = server.snapshot();
    assert_eq!(requests.len(), 5); // Initialized connection reused.
    assert_eq!(
        requests[4].body["params"]["arguments"]["session_id"],
        arguments["session_id"]
    );
}

#[tokio::test]
async fn disabled_search_is_unadvertised_and_cannot_make_requests() {
    let server = Server::start(|request| normal_reply(request, results())).await;
    let mut config = local_config(&server, Duration::from_secs(1));
    config.search_provider = SearchProvider::None;
    let config = config.into_arc();
    let registry = ToolRegistry::from_config(config.clone());
    assert!(
        registry
            .descriptions()
            .iter()
            .all(|d| d["function"]["name"] != "search_web")
    );
    assert!(
        registry
            .execute("search_web", &json!({"query":"Rust"}))
            .await
            .is_err()
    );
    assert!(
        SearchWebTool::new(config)
            .execute(&json!({"query":"Rust"}))
            .await
            .is_err()
    );
    assert!(server.snapshot().is_empty());
}

#[tokio::test]
async fn direct_fetch_keeps_url_user_agent_and_report_sources() {
    let server = Server::start(|_| Reply {
        status: 200,
        body: "<main><p>Rust async documentation.</p></main>".into(),
        sse: false,
        hold_open: false,
    })
    .await;
    let registry =
        ToolRegistry::from_config(local_config(&server, Duration::from_secs(1)).into_arc());
    let args = json!({"url":server.endpoint});
    let output = registry.execute("fetch_url", &args).await.unwrap();
    assert_eq!(output["url"], server.endpoint);
    assert!(
        output["content"]
            .as_str()
            .unwrap()
            .contains("Rust async documentation.")
    );
    let calls = [ToolCall {
        tool_name: "fetch_url".into(),
        input: args,
        output: ToolResult::ok(output),
        duration_ms: 0,
    }];
    let report = build_search_report("question", "answer", &calls);
    assert_eq!(report.sources[0].url, server.endpoint);
    assert!(
        server.snapshot()[0]
            .headers
            .to_lowercase()
            .contains("user-agent: websearchagent/1.0")
    );
}

#[tokio::test]
async fn sse_returns_matching_response_without_waiting_for_stream_close() {
    let server = Server::start(|request| {
        if request.body["method"] != "tools/call" { return normal_reply(request, results()); }
        let mut reply = normal_reply(request, results());
        reply.body = format!(": keepalive\r\n\r\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}}\r\n\r\nevent: message\r\ndata: {}\r\n\r\n", reply.body);
        reply.sse = true;
        reply.hold_open = true;
        reply
    }).await;
    let registry =
        ToolRegistry::from_config(local_config(&server, Duration::from_secs(1)).into_arc());
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        registry.execute("search_web", &json!({"query":"Rust"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["result_count"], 2);
}

#[tokio::test]
async fn text_content_empty_results_and_count_bounds() {
    let server = Server::start(|request| {
        normal_reply(
            request,
            json!({"content":[{"type":"text","text":results()["structuredContent"].to_string()}]}),
        )
    })
    .await;
    let registry =
        ToolRegistry::from_config(local_config(&server, Duration::from_secs(1)).into_arc());
    assert_eq!(
        registry
            .execute("search_web", &json!({"query":"Rust","count":0}))
            .await
            .unwrap()["result_count"],
        1
    );
    assert_eq!(
        registry
            .execute("search_web", &json!({"query":"Rust","count":100}))
            .await
            .unwrap()["result_count"],
        2
    );
    let empty =
        Server::start(|request| normal_reply(request, json!({"structuredContent":{"results":[]}})))
            .await;
    let registry =
        ToolRegistry::from_config(local_config(&empty, Duration::from_secs(1)).into_arc());
    assert_eq!(
        registry
            .execute("search_web", &json!({"query":"Rust"}))
            .await
            .unwrap()["result_count"],
        0
    );
    let before = empty.snapshot().len();
    for args in [json!({}), json!({"query":"  "}), json!({"query":15})] {
        assert!(registry.execute("search_web", &args).await.is_err());
    }
    assert_eq!(empty.snapshot().len(), before);
}

#[tokio::test]
async fn http_failures_are_classified_without_fallback() {
    for status in [401, 403, 429, 500, 503] {
        let server = Server::start(move |request| {
            let mut reply = normal_reply(request, results());
            if request.body["method"] == "tools/call" {
                reply.status = status;
            }
            reply
        })
        .await;
        let registry =
            ToolRegistry::from_config(local_config(&server, Duration::from_secs(1)).into_arc());
        let error = registry
            .execute("search_web", &json!({"query":"Rust"}))
            .await
            .unwrap_err();
        assert_eq!(matches!(error, ToolError::Retryable(_)), status >= 429);
        assert_eq!(server.snapshot().len(), 4); // No retry or alternative-provider request.
        assert_eq!(
            serde_json::to_value(ToolResult::from_error(&error)).unwrap()["status"],
            "error"
        );
    }
}

#[tokio::test]
async fn malformed_protocol_and_tool_errors_remain_errors() {
    for result in [
        json!({"isError":true,"content":[{"type":"text","text":"bad input"}]}),
        json!({"structuredContent":{"results":[{"url":"file:///etc/passwd","excerpts":[]}]}}),
        json!({"structuredContent":{"results":[{"url":"https://example.com","excerpts":[null]}]}}),
        json!({"content":[{"type":"text","text":"not JSON"}]}),
    ] {
        let server = Server::start(move |request| normal_reply(request, result.clone())).await;
        let registry =
            ToolRegistry::from_config(local_config(&server, Duration::from_secs(1)).into_arc());
        assert!(matches!(
            registry
                .execute("search_web", &json!({"query":"Rust"}))
                .await,
            Err(ToolError::NonRetryable(_))
        ));
    }
    for body in [
        "not JSON",
        "{\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{}}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32602,\"message\":\"bad arguments\"}}",
    ] {
        let server = Server::start(move |_| Reply {
            status: 200,
            body: body.into(),
            sse: false,
            hold_open: false,
        })
        .await;
        let registry =
            ToolRegistry::from_config(local_config(&server, Duration::from_secs(1)).into_arc());
        assert!(matches!(
            registry
                .execute("search_web", &json!({"query":"Rust"}))
                .await,
            Err(ToolError::NonRetryable(_))
        ));
    }
}

#[tokio::test]
async fn timeout_and_cancellation_do_not_poison_session_or_spawn_retries() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = Server::start(move |request| {
        let mut reply = normal_reply(request, results());
        if request.body["method"] == "tools/call" && counter.fetch_add(1, Ordering::SeqCst) < 2 {
            reply.body = " ".into();
            reply.hold_open = true;
        }
        reply
    })
    .await;
    let registry =
        ToolRegistry::from_config(local_config(&server, Duration::from_millis(100)).into_arc());
    let error = registry
        .execute("search_web", &json!({"query":"timeout"}))
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Retryable(_)));
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            registry.execute("search_web", &json!({"query":"cancel"}))
        )
        .await
        .is_err()
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        registry
            .execute("search_web", &json!({"query":"after cancellation"}))
            .await
            .unwrap()["result_count"],
        2
    );
    let requests = server.snapshot();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "initialize")
            .count(),
        1
    );
    let sessions: Vec<_> = requests
        .iter()
        .filter(|r| r.body["method"] == "tools/call")
        .map(|r| r.body["params"]["arguments"]["session_id"].clone())
        .collect();
    assert!(sessions.iter().all(|id| id == &sessions[0]));
}

#[tokio::test]
#[ignore = "Live anonymous Parallel request; run explicitly, without search credentials"]
async fn live_anonymous_search_through_registry() {
    // Run in an isolated process with no .env and only fixture inference/weather keys.
    let config = Config::from_env()
        .expect("set GEMINI_API_KEY and OPENWEATHER_API_KEY; unset search credentials");
    assert!(config.brave_api_key.is_empty());
    assert_eq!(config.search_provider, SearchProvider::Parallel);
    let registry = ToolRegistry::from_config(config.into_arc());
    let result = registry
        .execute(
            "search_web",
            &json!({"query":"Rust Tokio tutorial", "count":2}),
        )
        .await;
    match result {
        Ok(output) => {
            assert!(output["result_count"].as_u64().unwrap() > 0);
            for row in output["results"].as_array().unwrap() {
                assert!(row["url"].as_str().unwrap().starts_with("https://"));
                assert!(!row["snippet"].as_str().unwrap().is_empty());
            }
            println!("Live registry anonymous search: {}", output);
        }
        Err(error) => panic!("Live registry anonymous search: {error}"),
    }
}

#[tokio::test]
async fn explicit_brave_error_does_not_contact_parallel() {
    let server = Server::start(|_| Reply {
        status: 401,
        body: String::new(),
        sse: false,
        hold_open: false,
    })
    .await;
    let mut config = local_config(&server, Duration::from_secs(1));
    config.search_provider = SearchProvider::Brave;
    config.brave_api_key = "configured-brave-key".into();
    // Reject the actual Brave HTTPS CONNECT before any external connection.
    config.http_client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&server.endpoint).unwrap())
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let registry = ToolRegistry::from_config(config.into_arc());
    assert!(
        registry
            .execute("search_web", &json!({"query":"Rust"}))
            .await
            .is_err()
    );
    let requests = server.snapshot();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].line,
        "CONNECT api.search.brave.com:443 HTTP/1.1"
    );
    assert_eq!(requests[0].body, Value::Null);
}

#[tokio::test]
async fn concurrent_searches_share_discovery_and_keep_unique_request_ids() {
    let server = Server::start(|request| normal_reply(request, results())).await;
    let mut config = local_config(&server, Duration::from_secs(1));
    // An explicit Parallel choice must also win when credentials are saved.
    config.brave_api_key = "saved-brave-key".into();
    let registry = ToolRegistry::from_config(config.into_arc());
    let args = json!({"query":"Rust"});
    let (first, second) = tokio::join!(
        registry.execute("search_web", &args),
        registry.execute("search_web", &args)
    );
    assert!(first.is_ok() && second.is_ok());
    let requests = server.snapshot();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "initialize")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "tools/list")
            .count(),
        1
    );
    let calls: Vec<_> = requests
        .iter()
        .filter(|r| r.body["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].body["id"], calls[1].body["id"]);
    assert_eq!(
        calls[0].body["params"]["arguments"]["session_id"],
        calls[1].body["params"]["arguments"]["session_id"]
    );
    assert!(
        requests
            .iter()
            .all(|r| !r.headers.to_lowercase().contains("x-subscription-token"))
    );
}

#[tokio::test]
async fn result_cap_and_anonymous_quota_error_are_preserved() {
    let server = Server::start(|request| {
        let result = if request.body["params"]["arguments"]["objective"] == "quota" {
            json!({"isError":true,"content":[{"type":"text","text":"Anonymous quota exceeded; retry later"}]})
        } else {
            json!({"structuredContent":{"results":(0..12).map(|i| json!({"url":format!("https://example.com/{i}"), "excerpts":["source"]})).collect::<Vec<_>>()}})
        };
        normal_reply(request, result)
    }).await;
    let registry =
        ToolRegistry::from_config(local_config(&server, Duration::from_secs(1)).into_arc());
    assert_eq!(
        registry
            .execute("search_web", &json!({"query":"Rust","count":999}))
            .await
            .unwrap()["result_count"],
        10
    );
    let error = registry
        .execute("search_web", &json!({"query":"quota"}))
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Retryable(_)));
    assert!(error.to_string().contains("Anonymous quota exceeded"));
}

// These fixtures run the public agent loop with scripted model HTTP responses.
// They make real local HTTP requests; no Gemini inference is involved.
fn agent_reply(request: &Request) -> Reply {
    if request.line.contains("/page?") {
        return Reply {
            status: 200,
            body: "<p>Run-specific source content.</p>".into(),
            sse: false,
            hold_open: false,
        };
    }
    if !request.line.contains("/model ") {
        return normal_reply(request, results());
    }
    assert_eq!(request.body["model"], MODEL_NAME);
    let messages = request.body["messages"].as_array().unwrap();
    let question = messages[1]["content"].as_str().unwrap();
    let tool_messages: Vec<_> = messages.iter().filter(|m| m["role"] == "tool").collect();
    let disabled = request.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["function"]["name"] != "search_web");
    if disabled {
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("Web search is disabled")
        );
    }
    let call = |id: &str, name: &str, args: Value| {
        json!({
            "id": id, "type":"function", "function":{"name":name,"arguments":args.to_string()}
        })
    };
    let choice = match tool_messages.len() {
        0 => json!({"finish_reason":"tool_calls", "message":{"role":"assistant", "tool_calls":[
            call("first", "search_web", json!({"query":format!("{question} first")}))
        ]}}),
        1 => {
            let host = request
                .headers
                .lines()
                .find_map(|line| line.strip_prefix("host: "))
                .unwrap();
            json!({"finish_reason":"tool_calls", "message":{"role":"assistant", "tool_calls":[
                call("second", "search_web", json!({"query":format!("{question} second")})),
                call("page", "fetch_url", json!({"url":format!("http://{host}/page?run={question}")}))
            ]}})
        }
        3 => {
            // Assert actual tool outcomes were consumed into this run's history.
            for (i, m) in tool_messages.iter().enumerate() {
                let result: Value = serde_json::from_str(m["content"].as_str().unwrap()).unwrap();
                if i < 2 {
                    if disabled || question == "brave" {
                        assert_eq!(result["status"], "error");
                    } else {
                        assert_eq!(result["status"], "success");
                        let suffix = if i == 0 { "first" } else { "second" };
                        assert_eq!(result["data"]["query"], format!("{question} {suffix}"));
                        assert_eq!(result["data"]["results"][0]["title"], "Tokio");
                    }
                } else {
                    assert_eq!(result["status"], "success");
                    assert!(
                        result["data"]["content"]
                            .as_str()
                            .unwrap()
                            .contains("Run-specific source")
                    );
                }
            }
            json!({"finish_reason":"stop", "message":{"role":"assistant", "content":"Answer from the run's sources."}})
        }
        n => panic!("unexpected scripted history length: {n}"),
    };
    Reply::json(json!({"choices":[choice]}))
}

fn local_agent(config: Arc<Config>, server: &Server) -> SearchAgent {
    let mut agent = SearchAgent::new(config);
    agent.model_endpoint = server.endpoint.replace("/mcp", "/model");
    agent
}

async fn complete_run(agent: &SearchAgent, question: &str) {
    let result = tokio::time::timeout(Duration::from_secs(5), agent.run(question))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result.termination_reason,
        TerminationReason::Completed
    ));
    assert_eq!(result.tool_calls.len(), 3);
    let report = result.report.unwrap();
    assert_eq!(
        report.search_queries,
        vec![format!("{question} first"), format!("{question} second")]
    );
    assert_eq!(report.sources.len(), 1);
    assert!(
        report.sources[0]
            .url
            .ends_with(&format!("/page?run={question}"))
    );
}

fn assert_run_identities(server: &Server, interleaved: bool) {
    let requests = server.snapshot();
    let calls: Vec<_> = requests
        .iter()
        .filter(|r| r.body["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 4);
    let mut identities = Vec::new();
    for question in ["alpha", "beta"] {
        let related: Vec<_> = calls
            .iter()
            .filter(|r| {
                r.body["params"]["arguments"]["objective"]
                    .as_str()
                    .unwrap()
                    .starts_with(question)
            })
            .collect();
        assert_eq!(related.len(), 2);
        let id = related[0].body["params"]["arguments"]["session_id"]
            .as_str()
            .unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok());
        assert_eq!(related[1].body["params"]["arguments"]["session_id"], id);
        identities.push(id.to_string());
    }
    assert_ne!(identities[0], identities[1]);
    if interleaved {
        assert!(calls[..2].iter().all(|r| {
            r.body["params"]["arguments"]["objective"]
                .as_str()
                .unwrap()
                .ends_with("first")
        }));
        assert!(calls[2..].iter().all(|r| {
            r.body["params"]["arguments"]["objective"]
                .as_str()
                .unwrap()
                .ends_with("second")
        }));
    }
    // The shared transport still initializes once, with project headers and no key.
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.body["method"] == "initialize")
            .count(),
        1
    );
    for r in requests.iter().filter(|r| r.line == "POST /mcp HTTP/1.1") {
        let headers = r.headers.to_lowercase();
        assert!(headers.contains("user-agent: websearchagent/1.0"));
        assert!(!headers.contains("authorization:"));
        assert!(!headers.contains("x-subscription-token:"));
    }
    // fetch_url remains direct HTTP, without MCP accounting or session headers.
    let fetches: Vec<_> = requests
        .iter()
        .filter(|r| r.line.starts_with("GET /page?"))
        .collect();
    assert_eq!(fetches.len(), 2);
    assert!(
        fetches
            .iter()
            .all(|r| !r.headers.to_lowercase().contains("mcp-session-id:"))
    );
}

#[tokio::test]
async fn sequential_agent_runs_have_distinct_identities_and_within_run_continuity() {
    let server = Server::start(agent_reply).await;
    let agent = local_agent(
        local_config(&server, Duration::from_secs(2)).into_arc(),
        &server,
    );
    complete_run(&agent, "alpha").await;
    complete_run(&agent, "beta").await;
    assert_run_identities(&server, false);
}

#[tokio::test]
async fn interleaved_runs_on_one_agent_keep_their_own_identities() {
    let server =
        Server::start_gated(agent_reply, Some(Arc::new(tokio::sync::Barrier::new(2)))).await;
    let agent = local_agent(
        local_config(&server, Duration::from_secs(2)).into_arc(),
        &server,
    );
    tokio::join!(complete_run(&agent, "alpha"), complete_run(&agent, "beta"));
    assert_run_identities(&server, true);
}

#[tokio::test]
async fn interleaved_agents_sharing_config_keep_their_own_identities() {
    let server =
        Server::start_gated(agent_reply, Some(Arc::new(tokio::sync::Barrier::new(2)))).await;
    let config = local_config_with_selection(
        &server,
        Duration::from_secs(2),
        Some("parallel"),
        Some("saved-brave-key"),
    )
    .into_arc();
    let first = local_agent(config.clone(), &server);
    let second = local_agent(config, &server);
    tokio::join!(complete_run(&first, "alpha"), complete_run(&second, "beta"));
    assert_run_identities(&server, true);
}

#[tokio::test]
async fn agent_runs_preserve_configured_brave_and_disabled_search() {
    for selection in [None, Some("brave"), Some("none")] {
        let server = Server::start(|r| {
            if r.line.starts_with("CONNECT ") {
                return Reply {
                    status: 401,
                    body: String::new(),
                    sse: false,
                    hold_open: false,
                };
            }
            agent_reply(r)
        })
        .await;
        let mut config = local_config_with_selection(
            &server,
            Duration::from_secs(2),
            selection,
            Some("saved-brave-key"),
        );
        config.http_client = reqwest::Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::https(&server.endpoint).unwrap())
            .timeout(Duration::from_secs(2))
            .user_agent(USER_AGENT)
            .build()
            .unwrap();
        let agent = local_agent(config.into_arc(), &server);
        complete_run(&agent, "brave").await;
        let requests = server.snapshot();
        assert!(requests.iter().all(|r| r.body["method"].is_null()));
        let brave_calls: Vec<_> = requests
            .iter()
            .filter(|r| r.line.starts_with("CONNECT "))
            .collect();
        assert_eq!(
            brave_calls.len(),
            if selection == Some("none") { 0 } else { 2 }
        );
        assert!(
            brave_calls
                .iter()
                .all(|r| r.line == "CONNECT api.search.brave.com:443 HTTP/1.1")
        );
    }
}
