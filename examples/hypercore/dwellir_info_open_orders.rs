//! Query Dwellir's HyperCore Info Endpoint and open orders for a user.
//!
//! # Usage
//!
//! ```bash
//! export DWELLIR_NODE_HOST="dedicated-hyperliquid-...n.dwellir.com"
//! export DWELLIR_API_KEY="..."
//! cargo run --example dwellir_info_open_orders -- <USER_ADDRESS>
//! ```

use clap::Parser;
use hypersdk::{
    Address,
    hypercore::{dwellir, types::ClearinghouseState},
};
use url::Url;

#[derive(Parser)]
struct Args {
    /// User address to query.
    user: Address,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = dwellir::Config::from_env()?;
    let client = config.info_client()?;

    let open_orders_request = dwellir::DwellirInfoRequest::OpenOrders { user: args.user };
    let portfolio_request = dwellir::DwellirInfoRequest::PortfolioState {
        user: args.user,
        dex: None,
    };
    let ws_endpoint = config.ws_endpoint();
    let info_url = config.info_url();

    println!("Dwellir node host: {}", config.node_host());
    println!(
        "Dwellir WS endpoint: {}",
        redacted_endpoint(&ws_endpoint, "ws")
    );
    println!("Dwellir gRPC endpoint: {}", config.grpc_endpoint());
    println!(
        "Dwellir info endpoint: {}",
        redacted_endpoint(&info_url, "info")
    );
    println!("User: {:?}", args.user);
    println!(
        "Open orders info request: {}",
        serde_json::to_string(&open_orders_request)?
    );
    println!(
        "Portfolio info request: {}",
        serde_json::to_string(&portfolio_request)?
    );

    let orders = client.open_orders(args.user).await?;
    println!("Open orders: {}", orders.len());

    for order in orders.iter().take(5) {
        println!(
            "- {} {} sz={} limit_px={} oid={} cloid={:?}",
            order.coin, order.side, order.sz, order.limit_px, order.oid, order.cloid
        );
    }

    let portfolio = client.portfolio_state(args.user, None).await?;
    println!("Portfolio user abstraction: {}", portfolio.user_abstraction);
    print_clearinghouse_summary("Portfolio native perps", portfolio.clearinghouse_state.native());
    println!(
        "Portfolio spot balances: {}",
        portfolio.spot_clearinghouse_state.balances.len()
    );
    for balance in portfolio
        .spot_clearinghouse_state
        .balances
        .iter()
        .take(5)
    {
        println!(
            "- {} total={} hold={} available={} entry_ntl={}",
            balance.coin,
            balance.total,
            balance.hold,
            balance.available(),
            balance.entry_ntl
        );
    }

    Ok(())
}

fn print_clearinghouse_summary(label: &str, state: Option<&ClearinghouseState>) {
    let Some(state) = state else {
        println!("{label}: unavailable");
        return;
    };

    println!(
        "{}: account_value={} withdrawable={} positions={}",
        label,
        state.margin_summary.account_value,
        state.withdrawable,
        state.asset_positions.len()
    );
}

fn redacted_endpoint(url: &Url, last_segment: &str) -> String {
    let port = url.port().map_or(String::new(), |port| format!(":{port}"));
    format!(
        "{}://{}{}{}<API_KEY>/{last_segment}",
        url.scheme(),
        url.host_str().unwrap_or("<host>"),
        port,
        if url.path().starts_with('/') { "/" } else { "" },
    )
}
