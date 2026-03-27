use std::collections::HashMap;

use serde_json::Value;

use crate::{tools::Tool, types::ToolError};

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn descriptions(&self) -> Vec<Value> {
        self.tools.values()
            .map(|t| t.description())
            .collect()
    }

    pub async fn execute(
        &self,
        tool_name: &str,
        args: &Value,
    ) -> Result<Value, ToolError> {
        match self.tools.get(tool_name) {
            Some(tool) => tool.execute(args).await,
            None => Err(ToolError::NonRetryable(
                format!("unknown tool: {}", tool_name)
            )),
        }
    }
}