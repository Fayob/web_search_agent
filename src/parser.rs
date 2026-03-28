use crate::types::SearchReport;

/// Attempts to parse a model's final response into a SearchReport.
/// Tries three strategies in order — direct parse, code fence extraction,
/// brace extraction. Returns None if all strategies fail.
pub fn parse_search_report(content: &str) -> Option<SearchReport> {
    let content = content.trim();

    // direct parse — the model followed instructions perfectly
    if let Ok(report) = serde_json::from_str::<SearchReport>(content) {
        return Some(report);
    }

    // markdown code fence — model wrapped JSON in ```json ... ```
    if let Some(extracted) = extract_from_code_fence(content) {
        if let Ok(report) = serde_json::from_str::<SearchReport>(&extracted) {
            return Some(report);
        }
    }

    // brace extraction — model added text before/after the JSON
    if let Some(extracted) = extract_between_braces(content) {
        if let Ok(report) = serde_json::from_str::<SearchReport>(&extracted) {
            return Some(report);
        }
    }

    // All strategies failed — return None
    None
}

fn extract_from_code_fence(content: &str) -> Option<String> {
    // Match ```json ... ``` or ``` ... ```
    let start = content.find("```")?;
    let after_fence = &content[start + 3..];

    // Skip the language identifier if present (e.g. "json\n")
    let json_start = if after_fence.starts_with("json") {
        after_fence.find('\n')? + 1
    } else {
        // No language identifier — content starts right after ```
        after_fence.find('\n').unwrap_or(0)
    };

    let json_content = &after_fence[json_start..];
    let end = json_content.find("```")?;

    Some(json_content[..end].trim().to_string())
}

fn extract_between_braces(content: &str) -> Option<String> {
    let start = content.find('{')?;
    let end   = content.rfind('}')?;

    if end <= start {
        return None;
    }

    Some(content[start..=end].to_string())
}