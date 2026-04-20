//! Bid on the Hyperliquid gossip priority auction.
//!
//! This demonstrates how to query the current auction status and place a signed bid
//! using the SDK's `gossip_priority_bid` method.
//!
//! Usage:
//!     cargo run --example priority-fee-bid -- --max 50 --ip 1.2.3.4
//!
//! --max limits the maximum HYPE bid (converted to wei internally).
//!
//! Note: The fee is deducted from your **spot** HYPE balance and burned.

use clap::Parser;
use hypersdk::hypercore::types::GossipPriorityAuctionStatus;
use hypersdk::{hypercore, U256};
use rust_decimal::Decimal;

mod credentials;
use credentials::Credentials;

#[derive(clap::Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    #[command(flatten)]
    credentials: Credentials,
    /// Maximum HYPE to bid (in HYPE, not wei — 1 HYPE = 1e18 wei).
    #[arg(long)]
    max: Decimal,
    /// IP address to receive prioritized gossip data.
    #[arg(long)]
    ip: String,
    /// Slot index to bid on (0=highest priority, 4=lowest). Defaults to 0.
    #[arg(long, default_value = "0")]
    slot: u8,
}

fn print_auction_status(status: &GossipPriorityAuctionStatus) {
    println!("\nCurrent auction status (3-minute cycle):");
    println!(
        "{:<6} {:>14} {:>15} {}",
        "Slot", "Price (HYPE)", "Time Left", "Winner"
    );
    println!("{}", "-".repeat(55));
    for slot in &status.slots {
        let time = if slot.secs_remaining == 0 {
            "Expired".to_string()
        } else {
            format!("{}s", slot.secs_remaining)
        };
        println!(
            "{:<6} {:>14} {:>15} {}",
            slot.slot_id,
            slot.price,
            time,
            if slot.winner.is_empty() { "(none)" } else { &slot.winner }
        );
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = simple_logger::init_with_level(log::Level::Info);

    let args = Cli::parse();
    let signer = args.credentials.get()?;
    let client = hypercore::mainnet();
    let nonce = chrono::Utc::now().timestamp_millis() as u64;

    // 1. Query and display current auction status.
    println!("Fetching gossip priority auction status...");
    let status = client.gossip_priority_auction_status().await?;
    print_auction_status(&status);

    // 2. Build max_gas in wei from --max HYPE.
    // 1 HYPE = 1e18 wei.
    let max_hype: f64 = args.max.try_into()?;
    let max_gas = U256::from((max_hype * 1e18) as u128);

    println!(
        "\nBidding on slot {} for IP {} with max {} HYPE ({} wei)",
        args.slot,
        args.ip,
        args.max,
        max_gas
    );

    // 3. Submit the signed bid to /exchange.
    let resp = client
        .gossip_priority_bid(&signer, args.slot, &args.ip, max_gas, nonce, None, None)
        .await?;

    match &resp {
        hypercore::types::Response::Ok(hypercore::types::OkResponse::Default) => {
            println!("Bid submitted successfully.");
        }
        hypercore::types::Response::Err(err) => {
            println!("Bid failed: {}", err);
        }
        _ => {
            println!("Response: {resp:?}");
        }
    }

    // 4. Print updated status so the caller can verify.
    println!("\nRefreshing auction status...");
    let new_status = client.gossip_priority_auction_status().await?;
    print_auction_status(&new_status);

    Ok(())
}