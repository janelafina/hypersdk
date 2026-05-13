//! Subscribe to Dwellir's real-time trades stream for a coin.
//!
//! # Usage
//!
//! ```bash
//! export DWELLIR_NODE_HOST="dedicated-hyperliquid-...n.dwellir.com"
//! export DWELLIR_API_KEY="..."
//! cargo run --example dwellir_trades -- BTC
//! cargo run --example dwellir_trades -- BTC 0x1ed8d101622beaf192d06137dfb220851bcad9fa
//! ```

use std::env;

use alloy::primitives::Address;
use anyhow::Result;
use futures::StreamExt;
use hypersdk::hypercore::dwellir::{
    self, DwellirIncoming, DwellirSubscription, DwellirWsEvent,
};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    simple_logger::init_with_level(log::Level::Info).ok();

    let mut args = env::args().skip(1);
    let coin = args.next().unwrap_or_else(|| "BTC".to_string());
    let user = args.next().map(|raw| raw.parse::<Address>()).transpose()?;

    let mut ws = dwellir::ws_from_env()?;
    ws.subscribe(DwellirSubscription::Trades {
        coin: coin.clone(),
        user,
    });

    match user {
        Some(user) => eprintln!("Subscribing to Dwellir trades {coin} for {user}..."),
        None => eprintln!("Subscribing to Dwellir trades {coin}..."),
    }

    while let Some(event) = ws.next().await {
        match event {
            DwellirWsEvent::Connected => eprintln!("[connected]"),
            DwellirWsEvent::Disconnected => eprintln!("[disconnected - reconnecting]"),
            DwellirWsEvent::Message(DwellirIncoming::SubscriptionResponse(_)) => {
                eprintln!("[subscription confirmed]");
            }
            DwellirWsEvent::Message(DwellirIncoming::Trades(trades)) => {
                for trade in trades {
                    println!(
                        "{} {} {}@{} tid={} taker={} maker={} hash={}",
                        trade.time,
                        trade.side,
                        trade.sz,
                        trade.px,
                        trade.tid,
                        trade.taker_address(),
                        trade.maker_address(),
                        trade.hash
                    );
                }
            }
            DwellirWsEvent::Message(DwellirIncoming::L4Book(_)) => {}
        }
    }

    Ok(())
}
