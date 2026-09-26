//! Stream Dwellir's gRPC `StreamBbo` and print each coin's top of book.
//!
//! # Usage
//!
//! ```bash
//! export DWELLIR_NODE_HOST="dedicated-hyperliquid-...n.dwellir.com"
//! export DWELLIR_API_KEY="..."
//! # (or DWELLIR_GRPC_ENDPOINT="https://...:443" + DWELLIR_API_KEY)
//!
//! cargo run --example dwellir_bbo                    # BTC and ETH
//! cargo run --example dwellir_bbo -- SOL xyz:XYZ100  # up to 20 coins
//! ```

use std::env;

use anyhow::{Context, Result};
use hypersdk::hypercore::dwellir::{Bbo, BboRequest, stream_bbo};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let mut coins: Vec<String> = env::args().skip(1).collect();
    if coins.is_empty() {
        coins = vec!["BTC".to_string(), "ETH".to_string()];
    }
    let endpoint = match env::var("DWELLIR_NODE_HOST") {
        Ok(host) if !host.is_empty() => format!("https://{host}:443"),
        _ => env::var("DWELLIR_GRPC_ENDPOINT")
            .context("set DWELLIR_NODE_HOST or DWELLIR_GRPC_ENDPOINT")?,
    };
    let api_key = env::var("DWELLIR_API_KEY").ok();
    let mut stream = stream_bbo(&endpoint, api_key.as_deref(), &BboRequest::new(coins))
        .await?
        .into_inner();
    while let Some(update) = stream.message().await? {
        let bbo = Bbo::try_from(update)?;
        let side = |level: &Option<hypersdk::hypercore::types::BookLevel>| {
            level.as_ref().map_or_else(
                || "-".to_string(),
                |level| format!("{}@{} ({})", level.sz, level.px, level.n),
            )
        };
        println!(
            "{} block {} time {}: bid {} | ask {}",
            bbo.coin,
            bbo.block_number,
            bbo.time,
            side(&bbo.bid),
            side(&bbo.ask)
        );
    }
    Ok(())
}
