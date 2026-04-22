//! Subscribe to Dwellir's L4 order-book stream for a coin.
//!
//! # Usage
//!
//! Set the WebSocket endpoint (typically provided by Dwellir and already
//! containing your credentials) then run:
//!
//! ```bash
//! export DWELLIR_WS_ENDPOINT="wss://..."
//! cargo run --example dwellir_l4 -- BTC
//! ```

use std::env;

use anyhow::Result;
use futures::StreamExt;
use hypersdk::hypercore::dwellir::{
    self, DwellirIncoming, DwellirSubscription, L4Event, L4Message, RawBookDiff,
};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    simple_logger::init_with_level(log::Level::Info).ok();

    let coin = env::args().nth(1).unwrap_or_else(|| "BTC".to_string());

    let mut ws = dwellir::l4_from_env()?;
    ws.subscribe(DwellirSubscription::L4Book { coin: coin.clone() });
    eprintln!("Subscribing to Dwellir L4 {coin}...");

    while let Some(event) = ws.next().await {
        match event {
            L4Event::Connected => eprintln!("[connected]"),
            L4Event::Disconnected => eprintln!("[disconnected — reconnecting]"),
            L4Event::Message(DwellirIncoming::SubscriptionResponse(_)) => {
                eprintln!("[subscription confirmed]");
            }
            L4Event::Message(DwellirIncoming::L4Book(L4Message::Snapshot(snap))) => {
                eprintln!(
                    "snapshot {} @ height {}: {} bids, {} asks",
                    snap.coin,
                    snap.height,
                    snap.bids().len(),
                    snap.asks().len()
                );
            }
            L4Event::Message(DwellirIncoming::L4Book(L4Message::Updates(up))) => {
                for status in &up.order_statuses {
                    println!(
                        "[{}] {} status={} oid={} {} {}@{}",
                        up.height,
                        status.user,
                        status.status,
                        status.order.oid,
                        status.order.side,
                        status.order.sz,
                        status.order.limit_px
                    );
                }
                for diff in &up.book_diffs {
                    let what = match &diff.raw_book_diff {
                        RawBookDiff::New { sz } => format!("new sz={sz}"),
                        RawBookDiff::Update { orig_sz, new_sz } => {
                            format!("update {orig_sz} -> {new_sz}")
                        }
                        RawBookDiff::Modified { sz } => format!("modified sz={sz}"),
                        RawBookDiff::Remove => "remove".to_string(),
                    };
                    println!(
                        "[{}] diff {} oid={} px={} {}",
                        up.height, diff.coin, diff.oid, diff.px, what
                    );
                }
            }
        }
    }

    Ok(())
}
