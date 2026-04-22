//! Bounded smoke test for the Dwellir fills gRPC stream.
//!
//! Streams a handful of fill blocks then exits.
//!
//! ```bash
//! export DWELLIR_GRPC_ENDPOINT="https://hyperliquid.dwellir.com:443"
//! export DWELLIR_API_KEY="..."
//! cargo run --example dwellir_fills_smoke
//! ```

use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use hypersdk::hypercore::dwellir::{self, FillsEvent};
use tokio::time::timeout;

const MAX_BLOCKS: usize = 3;
const OVERALL_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    simple_logger::init_with_level(log::Level::Info).ok();

    let mut fills = dwellir::fills_from_env()?;
    println!("[smoke] streaming fills, capped at {MAX_BLOCKS} blocks");

    let result = timeout(OVERALL_TIMEOUT, async {
        let mut blocks = 0usize;
        while let Some(event) = fills.next().await {
            match event {
                FillsEvent::Connected => println!("[smoke] connected"),
                FillsEvent::Disconnected => println!("[smoke] disconnected"),
                FillsEvent::Message(block) => {
                    blocks += 1;
                    println!(
                        "[smoke] block #{} height={} fills={}",
                        blocks,
                        block.block_number,
                        block.events.len()
                    );
                    for (user, fill) in block.events.iter().take(3) {
                        let role = if fill.is_taker() { "taker" } else { "maker" };
                        let liq = if fill.is_liquidation() { " LIQ" } else { "" };
                        println!(
                            "        {} {} {} {}@{} fee={} ({}{})",
                            user, fill.coin, fill.side, fill.sz, fill.px, fill.fee, role, liq
                        );
                    }
                    if blocks >= MAX_BLOCKS {
                        return blocks;
                    }
                }
            }
        }
        blocks
    })
    .await;

    match result {
        Ok(n) => println!("[smoke] ok — received {n} blocks"),
        Err(_) => println!("[smoke] timed out after {:?}", OVERALL_TIMEOUT),
    }

    Ok(())
}
