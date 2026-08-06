//! Switch the trading subaccount to Standard abstraction mode with manually
//! positioned collateral, one verified step at a time.
//!
//! Subcommands:
//! - `status` — abstraction mode + spot/xyz balances for the subaccount
//! - `fund --usd X` — move X USDC of the subaccount's spot balance into its
//!   xyz-dex clearinghouse (master-signed sendAsset, from_sub_account)
//! - `set-standard` — set the subaccount's abstraction mode to Standard
//!   (agent-signed, vaultAddress = subaccount)
//! - `set-unified` — revert the subaccount's mode to UnifiedAccount
//! - `defund` — move the subaccount's withdrawable xyz balance back to spot
//!
//! Env: HYPERLIQUID_LIVE_ACCOUNT_ADDRESS (master, must match the key),
//! HYPERLIQUID_LIVE_PRIVATE_KEY, HYPERLIQUID_LIVE_SUBACCOUNT_ADDRESS.

use std::{env, str::FromStr};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use hypersdk::{
    Address,
    hypercore::{
        self, NonceHandler, PrivateKeySigner,
        types::{AbstractionMode, AssetTarget, SendAsset, SendToken},
    },
};
use rust_decimal::Decimal;

#[derive(Parser, Debug)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Status,
    Fund {
        #[arg(long)]
        usd: Decimal,
    },
    SetStandard,
    SetUnified,
    Defund,
    /// Close the subaccount's xyz:XYZ100 position with a reduce-only IOC
    /// at up to 1% through the current mid.
    Flatten,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    let master: Address = env::var("HYPERLIQUID_LIVE_ACCOUNT_ADDRESS")?
        .parse()
        .context("bad master address")?;
    let sub: Address = env::var("HYPERLIQUID_LIVE_SUBACCOUNT_ADDRESS")?
        .parse()
        .context("bad subaccount address")?;
    let signer =
        PrivateKeySigner::from_str(&env::var("HYPERLIQUID_LIVE_PRIVATE_KEY")?).context("bad key")?;
    if signer.address() != master {
        bail!(
            "signer {:?} does not match master {master:?}",
            signer.address()
        );
    }

    let client = hypercore::mainnet();
    let nonce = NonceHandler::default();

    match cli.cmd {
        Cmd::Status => {
            let mode = client.abstraction_mode(sub).await?;
            println!("sub abstraction mode: {mode}");
            let spot = client.user_balances(sub).await?;
            for b in spot.iter().filter(|b| b.coin == "USDC") {
                println!("sub spot USDC: total={} hold={}", b.total, b.hold);
            }
            let xyz = client
                .clearinghouse_state(sub, Some("xyz".into()))
                .await?;
            println!(
                "sub xyz clearinghouse: accountValue={} withdrawable={}",
                xyz.margin_summary.account_value, xyz.withdrawable
            );
        }
        Cmd::Fund { usd } => {
            if usd <= Decimal::ZERO || usd > Decimal::from(20_000) {
                bail!("refusing implausible fund amount {usd}");
            }
            let usdc = client
                .spot_tokens()
                .await?
                .into_iter()
                .find(|t| t.name == "USDC")
                .context("USDC token not found")?;
            println!("sendAsset: sub spot -> sub xyz clearinghouse, {usd} USDC");
            let n = nonce.next();
            client
                .send_asset(
                    &signer,
                    SendAsset {
                        destination: sub,
                        source_dex: AssetTarget::Spot,
                        destination_dex: AssetTarget::Dex("xyz".into()),
                        token: SendToken(usdc),
                        amount: usd,
                        from_sub_account: format!("{sub:#x}"),
                        nonce: n,
                    },
                    n,
                )
                .await
                .context("sendAsset failed")?;
            println!("transfer submitted ok");
        }
        Cmd::SetStandard => {
            println!("agentSetAbstraction(Standard) vaultAddress={sub:#x}");
            match client
                .agent_set_abstraction(&signer, AbstractionMode::Standard, nonce.next(), Some(sub), None)
                .await
            {
                Ok(()) => {}
                Err(err) => {
                    println!("agent-signed failed ({err}); trying user-signed userSetAbstraction");
                    client
                        .user_set_abstraction(&signer, sub, AbstractionMode::Standard, nonce.next())
                        .await
                        .context("user-signed set standard failed")?;
                }
            }
            println!("mode now: {}", client.abstraction_mode(sub).await?);
        }
        Cmd::SetUnified => {
            println!("agentSetAbstraction(UnifiedAccount) vaultAddress={sub:#x}");
            client
                .agent_set_abstraction(
                    &signer,
                    AbstractionMode::UnifiedAccount,
                    nonce.next(),
                    Some(sub),
                    None,
                )
                .await
                .context("set unified failed")?;
            println!("mode now: {}", client.abstraction_mode(sub).await?);
        }
        Cmd::Flatten => {
            use hypersdk::hypercore::types::{
                BatchOrder, OrderGrouping, OrderRequest, OrderResponseStatus, OrderTypePlacement,
                Side, TimeInForce,
            };
            let xyz = client
                .clearinghouse_state(sub, Some("xyz".into()))
                .await?;
            let pos = xyz
                .asset_positions
                .iter()
                .find(|p| p.position.coin == "xyz:XYZ100")
                .map(|p| p.position.szi)
                .unwrap_or(Decimal::ZERO);
            if pos == Decimal::ZERO {
                println!("position already flat");
                return Ok(());
            }
            let dex = client
                .perp_dexes()
                .await?
                .into_iter()
                .find(|d| d.name() == "xyz")
                .context("xyz dex not found")?;
            let market = client
                .perps_from(dex)
                .await?
                .into_iter()
                .find(|m| m.name == "xyz:XYZ100")
                .context("xyz:XYZ100 market not found")?;
            let mids = client.all_mids(Some("xyz".into())).await?;
            let mid = *mids.get("xyz:XYZ100").context("mid not found")?;
            let is_buy = pos < Decimal::ZERO;
            let slip = mid * Decimal::new(1, 2); // 1%
            let raw_px = if is_buy { mid + slip } else { mid - slip };
            let side = if is_buy { Side::Bid } else { Side::Ask };
            let limit_px = market
                .round_by_side(side, raw_px, false)
                .context("failed to round limit price")?;
            let size = pos.abs();
            println!(
                "flatten: {} {} xyz:XYZ100 reduce-only IOC at {} (mid {})",
                if is_buy { "BUY" } else { "SELL" },
                size,
                limit_px,
                mid
            );
            let n = nonce.next();
            let statuses = client
                .place(
                    &signer,
                    BatchOrder {
                        orders: vec![OrderRequest {
                            asset: market.index,
                            is_buy,
                            limit_px,
                            sz: size,
                            reduce_only: true,
                            order_type: OrderTypePlacement::Limit {
                                tif: TimeInForce::Ioc,
                            },
                            cloid: Default::default(),
                        }],
                        grouping: OrderGrouping::Na,
                        builder: None,
                    },
                    n,
                    Some(sub),
                    None,
                )
                .await
                .context("flatten order failed")?;
            for s in &statuses {
                println!("status: {s:?}");
            }
            if !statuses
                .iter()
                .any(|s| matches!(s, OrderResponseStatus::Filled { .. }))
            {
                bail!("flatten IOC did not fill — check position and retry");
            }
            println!("flattened");
        }
        Cmd::Defund => {
            let xyz = client
                .clearinghouse_state(sub, Some("xyz".into()))
                .await?;
            let w = xyz.withdrawable;
            if w <= Decimal::ZERO {
                bail!("nothing withdrawable in sub xyz clearinghouse");
            }
            let usdc = client
                .spot_tokens()
                .await?
                .into_iter()
                .find(|t| t.name == "USDC")
                .context("USDC token not found")?;
            println!("sendAsset: sub xyz clearinghouse -> sub spot, {w} USDC");
            let n = nonce.next();
            client
                .send_asset(
                    &signer,
                    SendAsset {
                        destination: sub,
                        source_dex: AssetTarget::Dex("xyz".into()),
                        destination_dex: AssetTarget::Spot,
                        token: SendToken(usdc),
                        amount: w,
                        from_sub_account: format!("{sub:#x}"),
                        nonce: n,
                    },
                    n,
                )
                .await
                .context("sendAsset failed")?;
            println!("transfer submitted ok");
        }
    }
    Ok(())
}
