use web_search_agent::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;

    println!("Open weather key starts with {}...", &config.openweather_api_key[..3]);
   Ok(())
}
