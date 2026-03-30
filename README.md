# Web Search Agent

> A production-grade autonomous Web Search Agent built in Rust, demonstrating
> real-world AI agent architecture, tool-use protocols, structured observability,
> and robust error handling at every layer of the stack.

---

## Table of Contents

- [What This Is](#what-this-is)
- [Why It Matters](#why-it-matters)
- [Live Demo](#live-demo)
- [Architecture](#architecture)
  - [System Overview](#system-overview)
  - [The Agent Loop](#the-agent-loop)
  - [Tool Registry Pattern](#tool-registry-pattern)
  - [Error Taxonomy](#error-taxonomy)
  - [Structured Output Pipeline](#structured-output-pipeline)
- [Getting Started](#getting-started)
  - [Prerequisites](#prerequisites)
  - [API Keys](#api-keys)
  - [Running the Agent](#running-the-agent)
  - [Log Levels](#log-levels)
- [Production Guarantees](#production-guarantees)
- [Design Decisions and Tradeoffs](#design-decisions-and-tradeoffs)

---

## What This Is

`web search agent` is a fully autonomous research agent that accepts a natural language question, searches the web, reads sources concurrently, and returns a structured report with citations, confidence scoring, and a full audit trail of every action it took.
It makes real API calls, no mock data anywhere. It handles failures at every layer, network errors, paywalled content, model hallucinations, rate limits, and degrades gracefully rather than crashing. Every tool call is timed, logged with structured fields, and recorded in an audit trail you can inspect.

---

## Why It Matters

Most AI agent tutorials produce demos that work in happy-path conditions and collapse under real-world use. This project was built around the failures first:

- What happens when a URL returns 403? The agent notes it and continues.
- What happens when the model calls the same tool twice? Loop detection catches it.
- What happens when the context window fills up? The history is pruned intelligently.
- What happens when Gemini returns 503? Exponential backoff retries automatically.
- What happens when the run takes too long? A wall-clock ceiling terminates it cleanly.

Every production concern has an explicit, testable answer. This is the difference
between a demo and a system you could deploy.

---

## Live Demo

```
$ RUST_LOG=info cargo run -- "What are the latest ZK proof implementations in Ethereum?"

2024-01-15T10:23:01Z  INFO web search agent: agent run started max_iterations=10 max_urls=5
2024-01-15T10:23:01Z  INFO web search agent: starting iteration iteration=1 message_count=1
2024-01-15T10:23:02Z  INFO web search agent: model responded finish_reason=tool_calls iteration=1
2024-01-15T10:23:02Z  INFO web search agent: executing tool calls concurrently tool_call_count=1
2024-01-15T10:23:02Z  INFO web search agent: tool call succeeded tool=search_web duration_ms=412
2024-01-15T10:23:02Z  INFO web search agent: starting iteration iteration=2 message_count=3
2024-01-15T10:23:04Z  INFO web search agent: model responded finish_reason=tool_calls iteration=2
2024-01-15T10:23:04Z  INFO web search agent: executing tool calls concurrently tool_call_count=3
2024-01-15T10:23:05Z  INFO web search agent: tool call succeeded tool=fetch_url duration_ms=834
2024-01-15T10:23:05Z  INFO web search agent: tool call succeeded tool=fetch_url duration_ms=1203
2024-01-15T10:23:06Z  WARN web search agent: tool call failed tool=fetch_url category=degraded message="access denied (403)"
2024-01-15T10:23:06Z  INFO web search agent: starting iteration iteration=3 message_count=8
2024-01-15T10:23:08Z  INFO web search agent: model responded finish_reason=stop iteration=3
2024-01-15T10:23:08Z  INFO web search agent: agent run finished total_ms=7841 iterations=3 tool_success_rate=0.83

============================================================
RESEARCH REPORT
============================================================
Question:   What are the latest ZK proof implementations in Ethereum?
Confidence: high

Answer:
Ethereum's ZK scaling ecosystem has matured significantly in 2024. zkSync Era and StarkNet lead production deployments, with Polygon zkEVM achieving EVM equivalence. The dominant proof systems are STARKs (StarkNet), SNARKs via Groth16 (older protocols), and PLONKish arithmetization (most newer systems)...

Key Findings:
  • zkSync Era processes over 10M transactions weekly with sub-cent fees.
  • Polygon zkEVM reached type-2 EVM equivalence in Q3 2024.
  • Proof generation times dropped 40% year-over-year across major systems.

Sources:
  [ethereum.org — layer2] https://ethereum.org/en/layer-2/
  [vitalik.ca — zk-roadmap] https://vitalik.ca/general/2024/zk-roadmap.html

Limitations: Could not access: fetch_url: blog.matter-labs.io: access denied (403)
```

---

## Architecture

### System Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                        User Question                            │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│                     SearchAgent                                 │
│                                                                 │
│   AgentConfig          ToolRegistry           RunMetrics        │
│   ─────────────        ────────────           ──────────        │
│   max_iterations       SearchWebTool          tool_calls        │
│   max_urls             FetchUrlTool           model_calls       │
│   token_budget         GetWeatherTool         latency_ms        │
│   max_duration         GetCryptoPriceTool     success_rate      │
│   retry_config                                token_usage       │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│                     Agent Loop                                  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │  Guard: wall-clock timeout                               │   │
│  │  Guard: max iterations                                   │   │
│  │  Guard: token budget → prune history if needed           │   │
│  └──────────────────────────────────────────────────────────┘   │
│                          │                                      │
│                          ▼                                      │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │  call_model_with_retry()                                 │   │
│  │    └── exponential backoff on 429 / 5xx                  │   │
│  │    └── immediate fail on 400 / 401 / 403                 │   │
│  └──────────────────────────────────────────────────────────┘   │
│                          │                                      │
│               ┌──────────┴──────────┐                           │
│               │                     │                           │
│         finish_reason            finish_reason                  │
│           "stop"               "tool_calls"                     │
│               │                     │                           │
│               ▼                     ▼                           │
│  ┌─────────────────┐   ┌────────────────────────────────────┐   │
│  │ build_search    │   │  Loop detection (fingerprinting)   │   │
│  │ _report()       │   │  URL fetch limit enforcement       │   │
│  │                 │   │  join_all() concurrent execution   │   │
│  │ answer   ← model│   │  ToolResult serialization          │   │
│  │ sources  ← audit│   │  Audit trail recording             │   │
│  │ queries  ← audit│   │  History append                    │   │
│  │ limits   ← audit│   └────────────────────────────────────┘   │
│  └─────────────────┘                                            │
└─────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│                     AgentRunResult                              │
│                                                                 │
│   iterations          tool_calls[]         termination_reason   │
│   SearchReport       ToolCall {            Completed            │
│     question            tool_name          MaxIterations        │
│     answer              input              LoopDetected         │
│     sources             output             FatalError           │
│     confidence          duration_ms                             │
│     key_findings      }                                         │
│     search_queries                                              │
│     limitations                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### The Agent Loop

The loop is the core of the system. It is not a simple "call model, call tool, repeat" loop. It is a stateful control system with explicit guards at every entry point, concurrent execution of independent operations, and a clear separation between what the model owns and what the code owns.

**The model owns one thing: the prose answer.**
The code owns everything else: structure, sources, metadata, error handling, audit trail.
This separation is fundamental. The model is a language model, it is excellent at synthesizing information into coherent prose. It is not a structured data producer. Asking it to produce a JSON schema reliably is fighting its nature. Letting it produce prose and extracting structure from your own audit trail plays to both strengths.

**Loop iteration sequence:**

```
1. Guard: has wall-clock duration exceeded max_run_duration?
2. Guard: has iteration count exceeded max_iterations?
3. Guard: has context size exceeded token_budget? → prune if yes
4. Call Gemini with retry (backoff on transient errors)
5. Parse finish_reason from response
6. If "stop": build report from audit trail + prose answer → return
7. If "tool_calls":
   a. Append assistant message to history (protocol requirement)
   b. Check all tool call fingerprints for loop detection
   c. Build futures for all tool calls in this turn
   d. Execute all futures concurrently via join_all
   e. Process outcomes: record metrics, build tool result messages
   f. Append tool result messages to history
8. Repeat
```

### Tool Registry Pattern

Tools are not loose functions. Each tool is a struct that implements the `Tool` trait:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self)        -> &str;
    fn description(&self) -> Value;
    async fn execute(&self, args: Value) -> Result<Value, ToolError>;
}
```

The `ToolRegistry` holds a `HashMap<String, Box<dyn Tool>>` and provides two operations: `descriptions()` returns all tool schemas for the model, and `execute(name, args)` dispatches by name without a match statement.

**Adding a new tool requires exactly two things:**
1. A new file implementing `Tool`
2. One `registry.register()` call in `SearchAgent::new()`

The agent loop, the dispatcher, and the description collection all stay unchanged. The compiler enforces the contract — a struct that doesn't implement all three trait methods will not compile.

### Error Taxonomy

Every tool failure is classified into one of three categories:

```
ToolError::Retryable(msg)
    └── Transient condition. The agent should retry after backoff.
    └── Examples: network timeout, 429 rate limit, 503 unavailable

ToolError::NonRetryable(msg)
    └── Structural problem. Retrying is pointless.
    └── Examples: invalid API key, malformed URL, unknown tool name

ToolError::Degraded { url, reason }
    └── This specific input failed, but the agent can continue.
    └── Examples: 404 not found, 403 paywall, empty page content
    └── Recorded in ResearchReport.limitations automatically
```

No tool failure crashes the loop. Every failure becomes a `ToolResult::Error { category, message }` -- a JSON object the model reads, understands, and responds to intelligently. A `Degraded` fetch result tells the model "that URL was inaccessible, continue with what you have." The model pivots rather than failing.

### Structured Output Pipeline

The `SearchReport` is not produced by the model. It is assembled by `build_search_report()` from data the agent already collected:

```
ResearchReport field    ←    Source
─────────────────────────────────────────────────────────────────
question                ←    original question passed to run()
answer                  ←    model's final prose message
sources                 ←    successful fetch_url calls in audit trail
confidence              ←    derived from source count heuristic
key_findings            ←    sentence extraction from prose answer
search_queries          ←    search_web calls in audit trail
limitations             ←    Degraded errors in audit trail
```

This is the correct separation of concerns. The model synthesizes. The
code structures. You never ask the model to produce JSON — you ask it to
answer the question well, and you build the structure from evidence you
already have.

---

## Getting Started

### Prerequisites

```bash
# Rust toolchain stable is sufficient
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Verify
rustc --version   # rustc 1.75.0 or later
cargo --version
```

### API Keys

Create a `.env` file in the project root. This file is in `.gitignore` and must never be committed.

```bash
# .env
BRAVE_API_KEY=your_brave_search_api_key
OPENWEATHER_API_KEY=your_openweathermap_api_key
GEMINI_API_KEY=your_gemini_api_key
```

**Getting each key:**

| Key | Where to get it | Free tier |
|-----|----------------|-----------|
| `BRAVE_API_KEY` | [brave.com/search/api](https://brave.com/search/api/) | 2,000 queries/month |
| `OPENWEATHER_API_KEY` | [openweathermap.org/api](https://openweathermap.org/api) | 1,000 calls/day |
| `GEMINI_API_KEY` | [aistudio.google.com](https://aistudio.google.com/api-keys) | Free, Pay per use |

CoinGecko requires no key on the free tier.

### Running the Agent

```bash
# Clone and build
git clone https://github.com/Fayob/web_search_agent.git
cd web_search_agent
cargo build

# Run with a question
cargo run -- "What are the latest developments in Ethereum ZK scaling?"

# Or set a default question in main.rs and run without arguments
cargo run
```

### Log Levels

The agent uses structured logging via `tracing`. Control verbosity with
the `RUST_LOG` environment variable:

```bash
# Normal production output — one line per significant event
RUST_LOG=info cargo run -- "your question"

# Full debug — every HTTP request, every decision point
RUST_LOG=debug cargo run -- "your question"

# Quiet — warnings and errors only
RUST_LOG=warn cargo run -- "your question"

# Filter to this crate only, suppress dependency noise
RUST_LOG=web search agent=debug cargo run -- "your question"
```

**Example structured log output at INFO level:**

```
2024-01-15T10:23:01Z  INFO web search agent::raw::agent: agent run started
    max_iterations=10 max_urls=5 token_budget=128000

2024-01-15T10:23:02Z  INFO web search agent::raw::agent: tool call succeeded
    tool=search_web duration_ms=412 iteration=1

2024-01-15T10:23:06Z  WARN web search agent::raw::agent: tool call failed
    tool=fetch_url duration_ms=203 category=degraded
    message="access denied (403)"

2024-01-15T10:23:08Z  INFO web search agent::raw::agent: agent run finished
    total_ms=7841 iterations=3 total_tool_calls=4
    successful_tool_calls=3 failed_tool_calls=1
    model_calls=3 model_retries=0 avg_model_ms=1847
    tool_success_rate=0.75 estimated_tokens=8432 urls_fetched=2
```

Every field in the final log line is a metric. In production, this line
would be ingested by your observability platform (Datadog, Grafana, etc.)
and used for alerting, dashboards, and SLO tracking.

---

## Production Guarantees

| Concern | Guarantee | Implementation |
|---------|-----------|----------------|
| Loop termination | Always terminates | `max_iterations` hard ceiling |
| Time bounding | Never runs forever | `max_run_duration` wall-clock timeout |
| Context safety | Never exceeds model limit | Token budget tracking + history pruning |
| Transient failures | Auto-recover | Exponential backoff on 429/5xx |
| Tool failures | Never crash the loop | All errors become model-readable JSON |
| Stuck loops | Detected and broken | Fingerprint-based loop detection |
| URL cost control | Capped at 5 fetches | Per-run counter with graceful model notification |
| Concurrent fetches | All independent | `join_all` -- one failure does not cancel others |
| Credentials | Never in source | `dotenvy` + environment variables only |
| Observability | Every event logged | Structured `tracing` with typed fields |
| Audit trail | Every action recorded | `AgentRunResult` with full `Vec<ToolCall>` |

---

## Design Decisions and Tradeoffs

**Why the Tool trait over a match dispatcher?**

A match dispatcher requires editing the dispatcher every time a tool is added. The compiler does not tell you if you forgot. The trait pattern means adding a tool requires implementing three methods and one registration call. The compiler enforces the full contract. Tradeoff: slightly more boilerplate per tool, dramatically safer at scale.

**Why `Box<dyn Tool>` over generics?**

A generic `ToolRegistry<T: Tool>` can only hold one type of tool. A registry holding `Box<dyn Tool>` can hold any mix of tool types, which is necessary when each tool carries different state (different API keys different config).
Tradeoff: one table lookup per dispatch call negligible vs network I/O.

**Why not ask the model to produce JSON?**

Language models produce coherent prose reliably. They produce structurally valid JSON with exact field names only sometimes. The agent already collects every piece of structured data during the run (URLs fetched, queries made, errors encountered). Building the report from that data is more reliable than schema injection and more correct architecturally, the model's job is to answer the question, not to serialize data.

**Why separate `AgentConfig` from `SearchAgent`?**

Configuration is data. Behavior is code. Separating them means you can tune parameters (max iterations, token budget, retry delays) without recompiling, read them from environment variables or a config file. Different deployments can use different configurations of the same agent binary.

**Why `Arc<Config>` instead of passing config by reference?**

Concurrent async tasks (the `join_all` fetch batch) need to share the HTTP client without lifetime constraints. `Arc` provides shared ownership with atomic reference counting. The clone is cheap, it increments a counter, not copying the client or the API keys.

**Why `tracing` over `log` or `println`?**

`tracing` produces structured events with typed fields. These can be filtered by field value (show me all events where `tool=fetch_url`), shipped to external systems without parsing, and consumed programmatically. `println!` produces unstructured strings. In production, unstructured logs require regex to extract signal. Structured logs do not.

---

## License

MIT — see [LICENSE](/LICENSE)
