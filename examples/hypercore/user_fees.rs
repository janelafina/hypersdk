//! Query user-specific fee rates on Hyperliquid.
//!
//! This example demonstrates how to retrieve a user's effective maker/taker rates
//! and active referral discount.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example user_fees -- <USER_ADDRESS>
//! ```

use clap::Parser;
use hypersdk::{Address, hypercore};

#[derive(Parser)]
struct Args {
    /// User address to query fee rates for
    user: Address,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let client = hypercore::mainnet();

    let fees = client.user_fees(args.user).await?;

    println!("User: {:?}", args.user);
    println!("Maker rate: {}", fees.maker_rate);
    println!("Taker rate: {}", fees.taker_rate);
    println!("Referral discount: {}", fees.referral_discount);

    Ok(())
}
