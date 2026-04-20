//! Gossip priority auction commands.
//!
//! ## What is this?
//!
//! Hyperliquid's gossip network runs **5 Dutch auction slots** (indices 0–4) for
//! read-priority ordering. When you win a slot, your node receives transaction data
//! ~10ms faster per slot level before non-winners see it. All 5 slots reset on the
//! same synchronized 3-minute schedule.
//!
//! The winning bid amount is **burned from your spot HYPE balance**. Any address may
//! bid on behalf of any IP address (the signer doesn't need to own the IP).
//!
//! ## How the Dutch auction works
//!
//! Each slot resets at the start of a cycle. The opening price is **10x the previous
//! cycle's winning price**, decreasing linearly over the 3 minutes. Minimum price is
//! **0.1 HYPE**. You pay the price at the moment your bid lands — if it's above the
//! current Dutch auction price, you win and the difference is refunded.
//!
//! Example: If the previous cycle's winning price for slot 0 was 0.05 HYPE, the next
//! cycle opens at 0.5 HYPE and decreases to 0.1 HYPE over 180 seconds.
//!
//! ## Current state visibility
//!
//! There is **no per-user query endpoint** to see "my bids" directly. After placing
//! a bid, run `hypecli prio status` and check the `Winner` column for your IP. If
//! your IP appears there, you won. Re-running status throughout the cycle shows
//! whether you've been outbid.
//!
//! <https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/priority-fees>

use std::io::Write as IoWrite;

use clap::{Args, Subcommand};
use hypersdk::{
    hypercore::{HttpClient, NonceHandler},
    U256,
};
use rust_decimal::Decimal;
use hypersdk::hypercore::types::{OkResponse, Response};

use crate::SignerArgs;
use crate::utils::find_signer_sync;

/// Gossip priority auction commands.
///
/// Run `hypecli prio status` first to see the current prices, time remaining, and
/// active winners for all 5 slots. Use those prices to decide your `--max` bid.
#[derive(Subcommand)]
pub enum PrioCmd {
    /// Query the current gossip priority auction status.
    ///
    /// Shows winning prices, time remaining, and current winner (IP) for all 5 slots.
    /// Use this to decide how much to bid before running `hypecli prio bid`.
    Status(StatusCmd),
    /// Place a signed bid on a gossip priority slot.
    ///
    /// The fee is deducted from your spot HYPE balance and burned. To verify you won,
    /// re-run `hypecli prio status` afterward and look for your IP in the Winner column.
    Bid(BidCmd),
}

impl PrioCmd {
    pub async fn run(self) -> anyhow::Result<()> {
        match self {
            Self::Status(cmd) => cmd.run().await,
            Self::Bid(cmd) => cmd.run().await,
        }
    }
}

/// Query the current gossip priority auction status.
///
/// ## Output fields
///
/// | Column         | Description                                                          |
/// |----------------|----------------------------------------------------------------------|
/// | Slot           | Index 0–4 (lower = higher priority, ~10ms faster per slot)          |
/// | Price (HYPE)   | Current Dutch auction price. Resets to 10× last winner each cycle.   |
/// | Time Left      | Seconds until the next 3-minute cycle resets.                         |
/// | Winner         | IP address of the current leader (if any).                           |
///
/// Run this before bidding to gauge competitive prices.
#[derive(Args)]
pub struct StatusCmd {}

impl StatusCmd {
    pub async fn run(self) -> anyhow::Result<()> {
        let client = HttpClient::new(hypersdk::hypercore::Chain::Mainnet);

        println!("Fetching gossip priority auction status...");
        let status = client.gossip_priority_auction_status().await?;

        // Determine cycle progress indicator
        let cycle_len = 180u64;
        let progress = status
            .slots
            .first()
            .map(|s| cycle_len.saturating_sub(s.secs_remaining))
            .unwrap_or(0);
        let progress_pct = (progress as f64 / cycle_len as f64 * 100.0).round() as u32;

        println!(
            "\nDutch auction status — {}% through 3-minute cycle",
            progress_pct
        );
        println!(
            "{:<6} {:>16} {:>12} {}",
            "Slot", "Price (HYPE)", "Time Left", "Winner"
        );
        println!("{}", "-".repeat(60));

        for slot in &status.slots {
            let time = if slot.secs_remaining == 0 {
                "Expired".to_string()
            } else {
                format!("{}s", slot.secs_remaining)
            };
            let winner = if slot.winner.is_empty() {
                "(none)".to_string()
            } else {
                slot.winner.clone()
            };

            // Highlight your slot (slot 0 is highest priority)
            let marker = if slot.slot_id == 0 { " ← top priority" } else { "" };
            println!(
                "{:<6} {:>16} {:>12} {}{}",
                slot.slot_id, slot.price, time, winner, marker
            );
        }

        println!(
            "\nTip: Run `hypecli prio status` again after bidding to verify your IP \
             appears as Winner."
        );

        Ok(())
    }
}

/// Place a signed bid on a gossip priority slot.
///
/// ## How to use
///
/// 1. Run `hypecli prio status` to see current prices.
/// 2. Choose a slot (default: 0, highest priority).
/// 3. Set `--max` to your maximum acceptable price in HYPE units.
/// 4. Set `--ip` to the IP address that will receive prioritized gossip.
///
/// ## How billing works
///
/// - You pay the **current Dutch auction price at submission time**, not your `--max`.
/// - If the current price ≤ `--max`, you win immediately and pay `(current price) × 1e18`
///   wei from your spot HYPE balance.
/// - If the current price > `--max`, your bid is placed but you don't win yet.
///   It stays active for the remainder of the cycle; if the price drops below your
///   max before the cycle ends, you win automatically.
/// - Winning bid amounts are **burned**, not transferred.
///
/// ## Verifying success
///
/// After submitting, run `hypecli prio status` and check the `Winner` column:
///
/// - If your IP appears → you won that slot
/// - If your IP does not appear → either you're still pending below the price, or
///   someone else bid higher
///
/// ## Units
///
/// `--max` is in **HYPE** (not wei). 1 HYPE = 10^18 wei.
///
/// ## Examples
///
/// ```bash
/// # Check prices first
/// hypecli prio status
///
/// # Bid 0.5 HYPE max on slot 0 (highest priority) for your public IP.
/// hypecli prio bid --private-key 0x... --max 0.5 --ip 203.0.113.42
///
/// # Lower-priority slot 2, reserve up to 1 HYPE.
/// hypecli prio bid --keystore hot_wallet --max 1.0 --ip 198.51.100.7 --slot 2
/// ```
#[derive(Args, derive_more::Deref)]
pub struct BidCmd {
    #[deref]
    #[command(flatten)]
    pub signer: SignerArgs,

    /// Maximum HYPE to bid, in HYPE units (not wei).
    ///
    /// You pay the current Dutch auction price at submission time — not this value —
    /// as long as it's ≥ the current price. Fees are deducted from your spot HYPE
    /// balance and burned.
    #[arg(long)]
    pub max: Decimal,

    /// IP address to receive prioritized gossip data.
    ///
    /// Any IP may be specified regardless of who signs the transaction. Enter your
    /// node's public IPv4/IPv6 address so the gossip peer can connect directly.
    #[arg(long)]
    pub ip: String,

    /// Slot index to bid on.
    ///
    /// Slots 0–4 exist. Lower index = higher priority (~10 ms latency advantage per
    /// slot level over non-winners). Defaults to slot 0 (top priority).
    ///
    /// | Slot | Priority offset vs no-bid |
    /// |------|---------------------------|
    /// | 0    | ~50 ms faster              |
    /// | 1    | ~40 ms faster              |
    /// | 2    | ~30 ms faster              |
    /// | 3    | ~20 ms faster              |
    /// | 4    | ~10 ms faster              |
    #[arg(long, default_value = "0")]
    pub slot: u8,
}

impl BidCmd {
    pub async fn run(self) -> anyhow::Result<()> {
        let signer = find_signer_sync(&self.signer)?;
        let client = HttpClient::new(self.chain);

        // Convert max HYPE to wei (1 HYPE = 1e18 wei).
        let max_hype: f64 = self
            .max
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid --max value: {}", self.max))?;
        let max_gas = U256::from((max_hype * 1e18) as u128);

        let nonce = NonceHandler::default().next();

        println!("Placing gossip priority bid:");
        println!("  Signer:     {}", signer.address());
        println!("  Slot:       {} ({})", self.slot, if self.slot == 0 { "top priority" } else { "" });
        println!("  Target IP:  {}", self.ip);
        println!("  Max bid:    {} HYPE ({} wei)", self.max, max_gas);
        println!("  Nonce:      {}", nonce);
        println!();

        let resp = client
            .gossip_priority_bid(
                &signer,
                self.slot,
                &self.ip,
                max_gas,
                nonce,
                None,
                None,
            )
            .await?;

        match &resp {
            Response::Ok(OkResponse::Default) => {
                println!("✓ Bid submitted successfully.");
                println!();
                println!("Verify your win by checking `hypecli prio status` —");
                println!("your IP {} should appear under slot {}.", self.ip, self.slot);
            }
            Response::Err(err) => {
                println!("✗ Bid failed: {err}");
            }
            _ => {
                println!("Unexpected response: {resp:?}");
            }
        }

        // Refresh and print updated status.
        println!("\nRefreshing auction status...");
        let new_status = client.gossip_priority_auction_status().await?;

        println!("\n{:<6} {:>16} {:>12} {}", "Slot", "Price (HYPE)", "Time Left", "Winner");
        println!("{}", "-".repeat(60));

        for slot in &new_status.slots {
            let time = if slot.secs_remaining == 0 {
                "Expired".to_string()
            } else {
                format!("{}s", slot.secs_remaining)
            };
            let winner = if slot.winner.is_empty() {
                "(none)".to_string()
            } else {
                slot.winner.clone()
            };

            // Emphasize the slot we're targeting
            if slot.slot_id == self.slot {
                writeln!(
                    std::io::stdout(),
                    "{:<6} {:>16} {:>12} {} ← target",
                    slot.slot_id, slot.price, time, winner
                )?;
            } else {
                writeln!(
                    std::io::stdout(),
                    "{:<6} {:>16} {:>12} {}",
                    slot.slot_id, slot.price, time, winner
                )?;
            }
        }

        Ok(())
    }
}