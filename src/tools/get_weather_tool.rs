use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{config::Config, tools::Tool, types::ToolError};

pub struct WeatherTool {
    config: Arc<Config>,
}

impl WeatherTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for WeatherTool {
    fn name(&self) -> &str {
        "get_weather"
    }

    fn description(&self) -> Value {
        json!({
            "name": self.name(),
            "description": "Get current weather conditions for a city. 
                        Use when the research question involves weather, 
                        climate, or current conditions in a location. 
                        Returns temperature, conditions, humidity, and wind speed.",
            "parameters": {
                "type": "object",
                "properties": {
                    "city": {
                        "type": "string",
                        "description": "City name, optionally with country code: 'London' or 'London,UK'"
                    }
                },
                "required": ["city"]
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<Value, ToolError> {
        let city = args["city"].as_str().ok_or_else(|| {
            ToolError::NonRetryable("missing required parameter: city".to_string())
        })?;

        get_weather(&self.config, city).await
    }
}


pub async fn get_weather(
    config: &Arc<Config>,
    city: &str,
) -> Result<Value, ToolError> {

    let url = format!(
        "https://api.openweathermap.org/data/2.5/weather?q={}&appid={}&units=metric",
        urlencoding::encode(city),
        config.openweather_api_key
    );

    let response = config
        .http_client
        .get(&url)
        .send()
        .await
        .map_err(|e| ToolError::Retryable(format!("weather request failed: {}", e)))?;

    let status = response.status();

    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ToolError::NonRetryable(
            "OpenWeatherMap API key is invalid".to_string()
        ));
    }

    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(ToolError::Degraded {
            url: format!("weather for {}", city),
            reason: format!("city '{}' not found", city),
        });
    }

    if !status.is_success() {
        return Err(ToolError::Retryable(
            format!("weather API error: {}", status)
        ));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| ToolError::NonRetryable(
            format!("failed to parse weather response: {}", e)
        ))?;

    // Extract and reshape — only what's useful for research
    Ok(json!({
        "city": city,
        "temperature_celsius": body["main"]["temp"],
        "feels_like_celsius":  body["main"]["feels_like"],
        "conditions":          body["weather"][0]["description"],
        "humidity_percent":    body["main"]["humidity"],
        "wind_speed_ms":       body["wind"]["speed"]
    }))
}