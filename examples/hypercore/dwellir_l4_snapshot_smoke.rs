//! Live seamless-resnapshot smoke test for Dwellir L4 capture.
//!
//! It records the primary hot stream while two isolated provider snapshots are
//! requested, interleaves only strictly newer updates, and proves the
//! continuously rebuilt book matches the second authoritative snapshot after
//! both reach the same update height.
//!
//! ```bash
//! export DWELLIR_NODE_HOST="dedicated-hyperliquid-...n.dwellir.com"
//! export DWELLIR_API_KEY="..."
//! cargo run --example dwellir_l4_snapshot_smoke -- BTC
//! ```

use std::{collections::VecDeque, env, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use hypersdk::hypercore::dwellir::{
    AuthoritativeL4Snapshot, Config, DwellirIncoming, DwellirSubscription, L4BookRecorder,
    L4Connection, L4Event, L4SnapshotClient, L4Updates,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const MAX_BUFFERED_UPDATES: usize = 100_000;
const OVERALL_TIMEOUT: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() -> Result<()> {
    simple_logger::init_with_level(log::Level::Info).ok();
    let coin = env::args().nth(1).unwrap_or_else(|| "BTC".to_owned());
    let config = Config::from_env()?;
    let client = config.l4_snapshot_client();
    let capabilities = client.capabilities();
    if !capabilities.authoritative_snapshot || !capabilities.snapshot_exchange_timestamp {
        bail!("strict authoritative snapshot capabilities are unavailable");
    }

    timeout(
        OVERALL_TIMEOUT,
        run_smoke(config.l4_connection(), client, coin),
    )
    .await
    .context("seamless L4 snapshot smoke test timed out")??;
    Ok(())
}

async fn run_smoke(
    mut primary: L4Connection,
    client: L4SnapshotClient,
    coin: String,
) -> Result<()> {
    primary.subscribe(DwellirSubscription::L4Book { coin: coin.clone() });
    println!("[snapshot-smoke] primary subscribed to {coin}");

    let (base, buffered) =
        capture_snapshot_while_recording(&client, &coin, &mut primary, None).await?;
    println!(
        "[snapshot-smoke] base height={} exchange_time_ms={} orders={} buffered_updates={}",
        base.height,
        base.exchange_time_ms,
        base.orders.len(),
        buffered.len()
    );
    let mut continuous = L4BookRecorder::new(&coin);
    continuous.apply_snapshot(base)?;
    apply_buffered(&mut continuous, &buffered)?;

    let (validation, during_validation) =
        capture_snapshot_while_recording(&client, &coin, &mut primary, Some(&mut continuous))
            .await?;
    println!(
        "[snapshot-smoke] validation height={} exchange_time_ms={} orders={} buffered_updates={}",
        validation.height,
        validation.exchange_time_ms,
        validation.orders.len(),
        during_validation.len()
    );
    let validation_height = validation.height;
    let mut resnapshotted = L4BookRecorder::new(&coin);
    resnapshotted.apply_snapshot(validation)?;
    apply_buffered(&mut resnapshotted, &during_validation)?;

    while continuous.height().unwrap_or_default() < validation_height
        || continuous.height() != resnapshotted.height()
    {
        let update = next_update(&mut primary).await?;
        continuous.apply_update(&update)?;
        resnapshotted.apply_update(&update)?;
    }

    if !continuous.has_same_resting_book(&resnapshotted) {
        bail!(
            "rebuilt book diverged at height {:?}: continuous_orders={} snapshot_orders={}",
            continuous.height(),
            continuous.len(),
            resnapshotted.len()
        );
    }
    println!(
        "[snapshot-smoke] ok height={} orders={} — primary stream and interleaved snapshot agree",
        continuous.height().unwrap_or_default(),
        continuous.len()
    );
    Ok(())
}

async fn capture_snapshot_while_recording(
    client: &L4SnapshotClient,
    coin: &str,
    primary: &mut L4Connection,
    mut recorder: Option<&mut L4BookRecorder>,
) -> Result<(AuthoritativeL4Snapshot, VecDeque<L4Updates>)> {
    let request = client.fetch_l4_snapshot_observation(coin, CancellationToken::new());
    tokio::pin!(request);
    let mut buffered = VecDeque::new();
    loop {
        tokio::select! {
            observation = &mut request => {
                let observation = observation?;
                println!(
                    "[snapshot-smoke] request={} authority={:?} time_source={:?} provider_operations={} receipt_time_ms={}",
                    observation.request_id,
                    observation.snapshot.authority,
                    observation.snapshot.exchange_time_source,
                    observation.provider_operations,
                    observation.receipt_time_ms,
                );
                return Ok((observation.snapshot, buffered));
            },
            event = primary.next() => {
                match event.ok_or_else(|| anyhow!("primary stream closed"))? {
                    L4Event::Connected => println!("[snapshot-smoke] primary connected"),
                    L4Event::Disconnected => bail!("primary stream disconnected during snapshot"),
                    L4Event::Message(DwellirIncoming::L4Book(
                        hypersdk::hypercore::dwellir::L4Message::Updates(update)
                    )) => {
                        if buffered.len() == MAX_BUFFERED_UPDATES {
                            bail!("bounded update buffer exhausted");
                        }
                        if let Some(book) = recorder.as_deref_mut() {
                            book.apply_update(&update)?;
                        }
                        buffered.push_back(update);
                    }
                    L4Event::Message(DwellirIncoming::Error(error)) => {
                        bail!("provider error on primary stream: {error}");
                    }
                    _ => {}
                }
            }
        }
    }
}

fn apply_buffered(book: &mut L4BookRecorder, updates: &VecDeque<L4Updates>) -> Result<()> {
    for update in updates {
        book.apply_update(update)?;
    }
    Ok(())
}

async fn next_update(primary: &mut L4Connection) -> Result<L4Updates> {
    loop {
        match primary
            .next()
            .await
            .ok_or_else(|| anyhow!("primary stream closed"))?
        {
            L4Event::Message(DwellirIncoming::L4Book(
                hypersdk::hypercore::dwellir::L4Message::Updates(update),
            )) => return Ok(update),
            L4Event::Disconnected => bail!("primary stream disconnected during validation"),
            L4Event::Message(DwellirIncoming::Error(error)) => {
                bail!("provider error on primary stream: {error}")
            }
            _ => {}
        }
    }
}
