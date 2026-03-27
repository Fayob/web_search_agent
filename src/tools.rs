pub mod fetch_url_tool;
pub mod get_crypto_price_tool;
pub mod get_weather_tool;
pub mod search_web_tool;

use async_trait::async_trait;
use serde_json::Value;

use crate::types::ToolError;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> Value;
    async fn execute(&self, args: &Value) -> Result<Value, ToolError>;
}
