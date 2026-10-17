use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::parallel_search::ParallelSearchClient;

pub const MODEL_NAME: &str = "gemini-2.5-flash";
pub const USER_AGENT: &str = "WebSearchAgent/1.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchProvider {
    Parallel,
    Brave,
    None,
}

impl SearchProvider {
    fn resolve(selection: Option<&str>, brave_key: &str) -> Result<Self> {
        match selection {
            Some("parallel") => Ok(Self::Parallel),
            Some("none") => Ok(Self::None),
            Some("brave") if !brave_key.trim().is_empty() => Ok(Self::Brave),
            Some("brave") => bail!("SEARCH_PROVIDER=brave requires BRAVE_API_KEY"),
            Some(_) => bail!("SEARCH_PROVIDER must be parallel, brave, or none"),
            None if !brave_key.trim().is_empty() => Ok(Self::Brave),
            None => Ok(Self::Parallel),
        }
    }
}

pub struct Config {
    pub brave_api_key: String,
    pub search_provider: SearchProvider,
    pub openweather_api_key: String,
    pub gemini_api_key: String,
    pub http_client: reqwest::Client,
    pub(crate) parallel_search: ParallelSearchClient,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    // A lookup keeps provider resolution testable without changing process-wide env.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let brave_api_key = lookup("BRAVE_API_KEY").unwrap_or_default();
        let search_provider =
            SearchProvider::resolve(lookup("SEARCH_PROVIDER").as_deref(), &brave_api_key)?;
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent(USER_AGENT)
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            brave_api_key,
            search_provider,
            openweather_api_key: lookup("OPENWEATHER_API_KEY")
                .context("OPENWEATHER_API_KEY not set")?,
            gemini_api_key: lookup("GEMINI_API_KEY").context("GEMINI_API_KEY not set")?,
            parallel_search: ParallelSearchClient::new(http_client.clone()),
            http_client,
        })
    }

    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}
