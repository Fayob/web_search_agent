use std::{collections::HashMap, sync::Arc};

use serde_json::Value;

use crate::{
    config::{Config, SearchProvider},
    tools::{
        Tool, fetch_url_tool::FetchURLTool, get_crypto_price_tool::GetCryptoPriceTool,
        get_weather_tool::GetWeatherTool, search_web_tool::SearchWebTool,
    },
    types::ToolError,
};

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn from_config(config: Arc<Config>) -> Self {
        let mut registry = Self::new();
        if config.search_provider != SearchProvider::None {
            registry.register(Box::new(SearchWebTool::new(config.clone())));
        }
        registry.register(Box::new(FetchURLTool::new(config.clone())));
        registry.register(Box::new(GetWeatherTool::new(config.clone())));
        registry.register(Box::new(GetCryptoPriceTool::new(config)));
        registry
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn descriptions(&self) -> Vec<Value> {
        self.tools.values().map(|t| t.description()).collect()
    }

    pub async fn execute(&self, tool_name: &str, args: &Value) -> Result<Value, ToolError> {
        match self.tools.get(tool_name) {
            Some(tool) => tool.execute(args).await,
            None => Err(ToolError::NonRetryable(format!(
                "unknown tool: {}",
                tool_name
            ))),
        }
    }
}
