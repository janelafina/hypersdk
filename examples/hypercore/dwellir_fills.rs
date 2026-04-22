//! Stream chain-wide fills from Dwellir over gRPC.
//!
//! # Usage
//!
//! ```bash
//! export DWELLIR_GRPC_ENDPOINT="https://hyperliquid.dwellir.com:443"
//! export DWELLIR_API_KEY="..."
//! cargo run --example dwellir_fills
//! ```

use anyhow::Result;
use futures::StreamExt;
use hypersdk::hypercore::dwellir::{self, FillsEvent};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    simple_logger::init_with_level(log::Level::Info).ok();

    let mut fills = dwellir::fills_from_env()?;
    eprintln!("Streaming Dwellir fills...");

    while let Some(event) = fills.next().await {
        match event {
            FillsEvent::Connected => eprintln!("[connected]"),
            FillsEvent::Disconnected => eprintln!("[disconnected — reconnecting]"),
            FillsEvent::Message(block) => {
                for (user, fill) in &block.events {
                    let role = if fill.is_taker() { "taker" } else { "maker" };
                    let liq = if fill.is_liquidation() { " LIQ" } else { "" };
                    println!(
                        "block {} {} {} {} {} @ {} fee={} {}{}",
                        block.block_number,
                        user,
                        fill.coin,
                        fill.side,
                        fill.sz,
                        fill.px,
                        fill.fee,
                        role,
                        liq,
                    );
                }
            }
        }
    }

    Ok(())
}
