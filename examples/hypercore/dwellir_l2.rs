//! Subscribe to Dwellir's aggregated L2 book (and optionally BBO) for a set
//! of coins and report per-coin push cadence — a throughput probe.
//!
//! # Usage
//!
//! ```bash
//! export DWELLIR_NODE_HOST="dedicated-hyperliquid-...n.dwellir.com"
//! export DWELLIR_API_KEY="..."
//! cargo run --example dwellir_l2 -- BTC,ETH,SOL 3 100 30
//! #                                  coins  nSigFigs nLevels seconds
//! ```

use std::collections::BTreeMap;
use std::env;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::StreamExt;
use hypersdk::hypercore::dwellir::{self, DwellirIncoming, DwellirSubscription, DwellirWsEvent};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    simple_logger::init_with_level(log::Level::Info).ok();

    let mut args = env::args().skip(1);
    let coins: Vec<String> = args
        .next()
        .unwrap_or_else(|| "BTC".to_string())
        .split(',')
        .map(str::to_owned)
        .collect();
    let n_sig_figs = args.next().and_then(|raw| raw.parse::<u8>().ok());
    let n_levels = args.next().and_then(|raw| raw.parse::<u8>().ok());
    let seconds = args
        .next()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(30);

    let mut ws = dwellir::ws_from_env()?;
    for coin in &coins {
        ws.subscribe(DwellirSubscription::L2Book {
            coin: coin.clone(),
            n_sig_figs,
            n_levels,
            strict: None,
        });
        ws.subscribe(DwellirSubscription::Bbo { coin: coin.clone() });
    }
    eprintln!(
        "Subscribing to Dwellir l2Book+bbo for {} coins (nSigFigs={n_sig_figs:?}, nLevels={n_levels:?}) for {seconds}s...",
        coins.len()
    );

    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let mut l2: BTreeMap<String, (u64, usize)> = BTreeMap::new();
    let mut bbo: BTreeMap<String, u64> = BTreeMap::new();
    while Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout(deadline - Instant::now(), ws.next()).await
        else {
            break;
        };
        match event {
            DwellirWsEvent::Connected => eprintln!("[connected]"),
            DwellirWsEvent::Disconnected => eprintln!("[disconnected — reconnecting]"),
            DwellirWsEvent::Message(DwellirIncoming::L2Book(book)) => {
                let entry = l2.entry(book.coin.clone()).or_default();
                entry.0 += 1;
                entry.1 += book.levels_len();
            }
            DwellirWsEvent::Message(DwellirIncoming::Bbo(quote)) => {
                *bbo.entry(quote.coin).or_default() += 1;
            }
            DwellirWsEvent::Message(DwellirIncoming::Error(error)) => {
                eprintln!("[provider error] {error}");
            }
            DwellirWsEvent::Message(_) => {}
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    println!("coin        l2/s   l2 bytes/msg   bbo/s");
    for (coin, (count, bytes)) in &l2 {
        let bbo_rate = bbo.get(coin).copied().unwrap_or(0) as f64 / elapsed;
        println!(
            "{coin:<10} {:>6.2} {:>12} {:>8.2}",
            *count as f64 / elapsed,
            bytes / (*count as usize).max(1),
            bbo_rate
        );
    }
    Ok(())
}
