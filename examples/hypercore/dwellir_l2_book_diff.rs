//! Stream Dwellir's gRPC `StreamL2BookDiff` and maintain local L2 books.
//!
//! # Usage
//!
//! ```bash
//! export DWELLIR_NODE_HOST="dedicated-hyperliquid-...n.dwellir.com"
//! export DWELLIR_API_KEY="..."
//! # (or DWELLIR_GRPC_ENDPOINT="https://...:443" + DWELLIR_API_KEY)
//!
//! # L2 diffs for BTC and ETH (default), printing top of book per update:
//! cargo run --example dwellir_l2_book_diff
//! cargo run --example dwellir_l2_book_diff -- BTC ETH SOL
//!
//! # Same program through the unified `BookConnection`, choosing L4 instead
//! # (L4 is single-coin; the first coin is used):
//! cargo run --example dwellir_l2_book_diff -- --l4 BTC
//! ```

use std::env;

use anyhow::Result;
use futures::StreamExt;
use hypersdk::hypercore::dwellir::{
    self, BookEvent, BookMessage, BookSubscription, L2BookDiffRequest, L2BookSet, L4Message,
};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    simple_logger::init_with_level(log::Level::Info).ok();

    let mut use_l4 = false;
    let mut coins = Vec::new();
    for arg in env::args().skip(1) {
        if arg == "--l4" {
            use_l4 = true;
        } else {
            coins.push(arg);
        }
    }
    if coins.is_empty() {
        coins = vec!["BTC".to_string(), "ETH".to_string()];
    }

    let subscription = if use_l4 {
        BookSubscription::l4(coins[0].clone())
    } else {
        BookSubscription::l2_diff(L2BookDiffRequest::new(coins.clone()))
    };
    eprintln!("Subscribing to {subscription:?}...");

    let mut books = dwellir::book_from_env(subscription)?;
    let mut l2 = match books.subscription() {
        BookSubscription::L2Diff(request) => L2BookSet::from_request(request),
        BookSubscription::L4 { .. } => L2BookSet::default(),
    };

    while let Some(event) = books.next().await {
        match event {
            BookEvent::Connected => eprintln!("[connected — awaiting snapshots]"),
            BookEvent::Disconnected => eprintln!("[disconnected — reconnecting]"),
            BookEvent::Error(err) => {
                eprintln!("[error] {err}");
                if err.is_terminal() {
                    break;
                }
            }
            BookEvent::Message(BookMessage::L2(update)) => match l2.apply(&update) {
                Ok(changed) => {
                    for (coin, outcome) in changed {
                        let book = l2.get(&coin).expect("tracked coin");
                        let bid = book
                            .best_bid()
                            .map_or("-".to_string(), |l| format!("{}x{}", l.sz, l.px));
                        let ask = book
                            .best_ask()
                            .map_or("-".to_string(), |l| format!("{}x{}", l.sz, l.px));
                        println!(
                            "[{}] {coin:>6} seq={:<6} {outcome:?} bid {bid} | ask {ask} ({} bids / {} asks)",
                            update.block_number,
                            book.seq().unwrap_or_default(),
                            book.bids().len(),
                            book.asks().len(),
                        );
                    }
                }
                Err(err) => {
                    eprintln!("[book error] {err}; resubscribing for fresh snapshots");
                    books.reconnect();
                }
            },
            BookEvent::Message(BookMessage::L4(L4Message::Snapshot(snap))) => {
                println!(
                    "[{}] {} L4 snapshot: {} bids / {} asks",
                    snap.height,
                    snap.coin,
                    snap.bids().len(),
                    snap.asks().len()
                );
            }
            BookEvent::Message(BookMessage::L4(L4Message::Updates(up))) => {
                println!(
                    "[{}] L4 update: {} statuses, {} book diffs",
                    up.height,
                    up.order_statuses.len(),
                    up.book_diffs.len()
                );
            }
        }
    }

    Ok(())
}
