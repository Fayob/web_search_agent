use crate::types::{SearchReport, Source, ToolCall, ToolResult};

pub fn build_search_report(
    question: &str,
    answer: &str,
    tool_calls_log: &[ToolCall],
) -> SearchReport {
    SearchReport {
        question: question.to_string(),
        answer: answer.to_string(),
        sources: extract_sources(tool_calls_log),
        confidence: derive_confidence(tool_calls_log),
        key_findings: extract_key_findings(answer),
        search_queries: extract_search_queries(tool_calls_log),
        limitations: extract_limitations(tool_calls_log),
    }
}

/// Extracts sources from successful fetch_url calls.
/// A source is a URL we successfully fetched and read.
fn extract_sources(tool_calls_log: &[ToolCall]) -> Vec<Source> {
    tool_calls_log
        .iter()
        .filter(|tc| tc.tool_name == "fetch_url")
        .filter_map(|tc| {
            match &tc.output {
                ToolResult::Success { data } => {
                    let url = data["url"].as_str().unwrap_or("").to_string();

                    if url.is_empty() {
                        return None;
                    }

                    let snippet = data["content"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(300)
                        .collect::<String>();

                    let title = url_to_title(&url);

                    Some(Source {
                        title,
                        url,
                        snippet,
                    })
                }
                ToolResult::Error { .. } => None,
            }
        })
        .collect()
}

/// Derives a readable title from a URL.
/// "https://vitalik.ca/posts/zk-proofs" → "vitalik.ca — zk-proofs"
fn url_to_title(url: &str) -> String {
    let without_protocol = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");

    let mut parts = without_protocol.splitn(2, '/');
    let domain = parts.next().unwrap_or(without_protocol);
    let path = parts.next().unwrap_or("");

    let last_segment = path
        .trim_end_matches('/')
        .split('/')
        .last()
        .unwrap_or("")
        .replace('-', " ")
        .replace('_', " ");

    if last_segment.is_empty() {
        domain.to_string()
    } else {
        format!("{} — {}", domain, last_segment)
    }
}

/// Derives confidence from how many sources we successfully gathered.
/// Simple heuristic — can be made more sophisticated later.
fn derive_confidence(tool_calls_log: &[ToolCall]) -> String {
    let successful_fetches = tool_calls_log
        .iter()
        .filter(|tc| tc.tool_name == "fetch_url")
        .filter(|tc| matches!(&tc.output, ToolResult::Success { .. }))
        .count();

    let failed_fetches = tool_calls_log
        .iter()
        .filter(|tc| tc.tool_name == "fetch_url")
        .filter(|tc| matches!(&tc.output, ToolResult::Error { .. }))
        .count();

    match (successful_fetches, failed_fetches) {
        (s, _) if s >= 3 => "high".to_string(),
        (s, _) if s >= 1 => "medium".to_string(),
        _ => "low".to_string(),
    }
}

/// Extracts the search queries that were actually used.
fn extract_search_queries(tool_calls_log: &[ToolCall]) -> Vec<String> {
    tool_calls_log
        .iter()
        .filter(|tc| tc.tool_name == "search_web")
        .filter_map(|tc| tc.input["query"].as_str().map(|q| q.to_string()))
        .collect()
}

/// Extracts limitations from Degraded tool failures.
/// These are the things the agent tried but couldn't access.
fn extract_limitations(tool_calls_log: &[ToolCall]) -> String {
    let failed: Vec<String> = tool_calls_log
        .iter()
        .filter_map(|tc| match &tc.output {
            ToolResult::Error { category, message } if category == "degraded" => {
                Some(format!("{}: {}", tc.tool_name, message))
            }
            _ => None,
        })
        .collect();

    if failed.is_empty() {
        "No limitations — all sources were accessible.".to_string()
    } else {
        format!("Could not access: {}", failed.join("; "))
    }
}

/// Extracts key findings by splitting the answer into sentences
/// and taking the most substantive ones.
///
/// This is a simple heuristic. A more sophisticated version would
/// use a second model call with a focused prompt:
/// "Extract 3-5 key findings from this text as bullet points"
fn extract_key_findings(answer: &str) -> Vec<String> {
    let sentences: Vec<&str> = answer
        .split(|c| c == '.' || c == '\n')
        .map(|s| s.trim())
        .filter(|s| {
            s.len() > 40
        })
        .take(5)
        .collect();

    sentences
        .into_iter()
        .map(|s| {
            if s.ends_with('.') {
                s.to_string()
            } else {
                format!("{}.", s)
            }
        })
        .collect()
}
