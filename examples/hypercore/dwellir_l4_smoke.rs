//! Bounded smoke test for the Dwellir L4 WebSocket stream.
//!
//! Subscribes to the L4 book for one coin, prints up to a handful of events
//! (including the snapshot), then exits.
//!
//! ```bash
//! export DWELLIR_WS_ENDPOINT="wss://..."
//! cargo run --example dwellir_l4_smoke -- BTC
//! ```

use std::env;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use hypersdk::hypercore::dwellir::{
    self, DwellirIncoming, DwellirSubscription, L4Event, L4Message, RawBookDiff,
};
use tokio::time::timeout;

const MAX_UPDATE_BATCHES: usize = 3;
const OVERALL_TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::main]
async fn main() -> Result<()> {
    simple_logger::init_with_level(log::Level::Info).ok();

    let coin = env::args().nth(1).unwrap_or_else(|| "BTC".to_string());
    let mut ws = dwellir::l4_from_env()?;
    ws.subscribe(DwellirSubscription::L4Book { coin: coin.clone() });

    println!("[smoke] subscribing L4 {coin}, capped at {MAX_UPDATE_BATCHES} update batches");

    let result = timeout(OVERALL_TIMEOUT, async {
        let mut saw_snapshot = false;
        let mut update_batches = 0usize;
        while let Some(event) = ws.next().await {
            match event {
                L4Event::Connected => println!("[smoke] connected"),
                L4Event::Disconnected => println!("[smoke] disconnected"),
                L4Event::Message(DwellirIncoming::SubscriptionResponse(_)) => {
                    println!("[smoke] subscription ack");
                }
                L4Event::Message(DwellirIncoming::L4Book(L4Message::Snapshot(snap))) => {
                    println!(
                        "[smoke] snapshot {} height={} bids={} asks={}",
                        snap.coin,
                        snap.height,
                        snap.bids().len(),
                        snap.asks().len()
                    );
                    if let Some(b) = snap.bids().first() {
                        println!("        top bid oid={} px={} sz={}", b.oid, b.limit_px, b.sz);
                    }
                    if let Some(a) = snap.asks().first() {
                        println!("        top ask oid={} px={} sz={}", a.oid, a.limit_px, a.sz);
                    }
                    saw_snapshot = true;
                }
                L4Event::Message(DwellirIncoming::L4Book(L4Message::Updates(up))) => {
                    update_batches += 1;
                    println!(
                        "[smoke] updates #{} height={} statuses={} diffs={}",
                        update_batches,
                        up.height,
                        up.order_statuses.len(),
                        up.book_diffs.len()
                    );
                    for s in up.order_statuses.iter().take(2) {
                        println!("        status {} oid={} side={} sz={}", s.status, s.order.oid, s.order.side, s.order.sz);
                    }
                    for d in up.book_diffs.iter().take(2) {
                        let what = match &d.raw_book_diff {
                            RawBookDiff::New { sz } => format!("new sz={sz}"),
                            RawBookDiff::Update { orig_sz, new_sz } => format!("update {orig_sz}->{new_sz}"),
                            RawBookDiff::Modified { sz } => format!("modified sz={sz}"),
                            RawBookDiff::Remove => "remove".to_string(),
                        };
                        println!("        diff oid={} px={} {}", d.oid, d.px, what);
                    }
                    if update_batches >= MAX_UPDATE_BATCHES {
                        return saw_snapshot;
                    }
                }
                L4Event::Message(DwellirIncoming::Trades(_)) => {}
            }
        }
        saw_snapshot
    })
    .await;

    match result {
        Ok(saw) => println!("[smoke] ok — saw snapshot: {saw}"),
        Err(_) => println!("[smoke] timed out after {:?}", OVERALL_TIMEOUT),
    }

    Ok(())
}
