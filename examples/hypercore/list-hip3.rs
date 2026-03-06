use hypersdk::hypercore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = hypercore::mainnet();

    let dexes = client.perp_dexs().await?;
    for dex in dexes {
        println!("\n\nmarkets for {dex}");
        println!("deployer fee scale: {:?}", dex.deployer_fee_scale());

        let markets = client.perps_from(dex).await?;
        for market in markets {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                market.name,
                market.index,
                market.name,
                market.collateral,
                market.growth_mode,
                market.aligned_quote_token
            );
        }
    }

    Ok(())
}
