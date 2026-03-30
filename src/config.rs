use std::sync::Arc;

use anyhow::{Context, Result};

pub struct Config {
    pub brave_api_key: String,
    pub openweather_api_key: String,
    pub gemini_api_key: String,
    pub tavily_api_key: Option<String>,
    pub http_client: reqwest::Client,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        Ok(Self {
            brave_api_key: std::env::var("BRAVE_API_KEY")
                .context("BRAVE_API_KEY not set")?,
            openweather_api_key: std::env::var("OPENWEATHER_API_KEY")
                .context("OPENWEATHER_API_KEY not set")?,
            gemini_api_key: std::env::var("GEMINI_API_KEY")
                .context("GEMINI_API_KEY not set")?,
            tavily_api_key: std::env::var("TAVILY_API_KEY").ok(),
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .user_agent("WebSearchAgent/1.0")
                .build()
                .context("failed to build HTTP clients")?,
        })
    }

    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}
