use std::sync::Arc;

use serde_json::{Value, json};

use crate::{config::Config, types::ToolError};

pub fn get_crypto_price_description() -> Value {
    json!({
        "name": "get_crypto_price",
        "description": "Get the current price and 24h stats for a cryptocurrency. 
                    Use for research questions about crypto markets, prices, or 
                    market cap. Pass the CoinGecko coin ID: 'bitcoin', 'ethereum', 
                    'solana' etc.",
        "parameters": {
            "type": "object",
            "properties": {
                "coin_id": {
                    "type": "string",
                    "description": "CoinGecko coin ID in lowercase: 'bitcoin', 
                                'ethereum', 'solana', 'cardano'"
                }
            },
            "required": ["coin_id"]
        }
    })
}

pub async fn get_crypto_price(
    config: Arc<Config>,
    coin_id: &str,
) -> Result<Value, ToolError> {

    let url = format!(
        "https://api.coingecko.com/api/v3/simple/price?ids={}&vs_currencies=usd&include_24hr_change=true&include_market_cap=true",
        urlencoding::encode(coin_id)
    );

    let response = config
        .http_client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| ToolError::Retryable(
            format!("crypto price request failed: {}", e)
        ))?;

    let status = response.status();

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(ToolError::Retryable(
            "CoinGecko rate limit hit".to_string()
        ));
    }

    if !status.is_success() {
        return Err(ToolError::Retryable(
            format!("CoinGecko API error: {}", status)
        ));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| ToolError::NonRetryable(
            format!("failed to parse CoinGecko response: {}", e)
        ))?;

    // CoinGecko response: { "bitcoin": { "usd": 45000, "usd_24h_change": 2.3 } }
    let coin_data = body.get(coin_id).ok_or_else(|| ToolError::Degraded {
        url: format!("coingecko:{}", coin_id),
        reason: format!("coin '{}' not found — check the CoinGecko coin ID", coin_id),
    })?;

    Ok(json!({
        "coin_id":          coin_id,
        "price_usd":        coin_data["usd"],
        "change_24h_pct":   coin_data["usd_24h_change"],
        "market_cap_usd":   coin_data["usd_market_cap"]
    }))
}
