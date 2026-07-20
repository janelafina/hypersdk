//! Live snapshot/update cutover smoke test using `l4-book`.
//!
//! The test builds one book from an authoritative snapshot plus every primary
//! stream update whose height is after the first snapshot and at or before the
//! second snapshot height.
//! It then asserts that the complete state, including FIFO order, equals a
//! second authoritative snapshot.
//!
//! ```bash
//! export DWELLIR_NODE_HOST="dedicated-hyperliquid-...n.dwellir.com"
//! export DWELLIR_API_KEY="..."
//! cargo run --example dwellir_l4_book_snapshot_smoke -- BTC
//! ```

use std::{collections::HashSet, env, time::Duration};

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures::StreamExt;
use hypersdk::hypercore::{
    dwellir::{
        AuthoritativeL4Snapshot, Config, DwellirIncoming, DwellirSubscription, L4BookDiff,
        L4Connection, L4Event, L4Message, L4Order, L4SnapshotClient, L4Updates, RawBookDiff,
    },
    types::Side as HyperSide,
};
use l4_book::{BookOp, Order, OrderBook, Side, WalletId, dwellir::Scales};
use rust_decimal::Decimal;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const MAX_BUFFERED_UPDATES: usize = 100_000;
const OVERALL_TIMEOUT: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() -> Result<()> {
    simple_logger::init_with_level(log::Level::Info).ok();
    let coin = env::args().nth(1).unwrap_or_else(|| "BTC".to_owned());
    let scales = scales_from_env()?;
    let config = Config::from_env()?;
    let client = config.l4_snapshot_client();
    let capabilities = client.capabilities();
    ensure!(
        capabilities.authoritative_snapshot && capabilities.snapshot_exchange_timestamp,
        "strict authoritative snapshot capabilities are unavailable"
    );

    timeout(
        OVERALL_TIMEOUT,
        run_smoke(config.l4_connection(), client, coin, scales),
    )
    .await
    .context("l4-book snapshot cutover smoke test timed out")??;
    Ok(())
}

async fn run_smoke(
    mut primary: L4Connection,
    client: L4SnapshotClient,
    coin: String,
    scales: Scales,
) -> Result<()> {
    primary.subscribe(DwellirSubscription::L4Book { coin: coin.clone() });
    println!("[l4-book-smoke] primary subscribed to {coin}");

    let (base, mut updates) = capture_snapshot(&client, &coin, &mut primary).await?;
    println!(
        "[l4-book-smoke] base height={} exchange_time_ms={} orders={} buffered_updates={}",
        base.height,
        base.exchange_time_ms,
        base.orders.len(),
        updates.len()
    );

    // Force observable progress before requesting the validation snapshot.
    while !updates.iter().any(|update| update.height > base.height) {
        push_bounded(&mut updates, next_update(&mut primary).await?)?;
    }

    let (validation, during_validation) = capture_snapshot(&client, &coin, &mut primary).await?;
    updates.extend(during_validation);
    ensure!(
        validation.height > base.height,
        "validation snapshot did not advance: base={} validation={}",
        base.height,
        validation.height
    );

    // The temporary snapshot connection can outrun the primary connection.
    // Seeing height >= the cutover proves all earlier primary messages have
    // crossed the local receive boundary before we compare state.
    while !updates
        .iter()
        .any(|update| update.height >= validation.height)
    {
        push_bounded(&mut updates, next_update(&mut primary).await?)?;
    }
    assert_monotonic_heights(&updates)?;

    let mut rebuilt = book_from_snapshot(&base, scales)?;
    let mut applied_batches = 0usize;
    let mut applied_ops = 0usize;
    for update in updates
        .iter()
        .filter(|update| belongs_to_cutover(base.height, validation.height, update.height))
    {
        applied_ops += apply_update(&mut rebuilt, update, &coin, scales)
            .with_context(|| format!("applying update at height {}", update.height))?;
        applied_batches += 1;
    }

    let expected = book_from_snapshot(&validation, scales)?;
    let rebuilt_state = complete_state(&rebuilt);
    let expected_state = complete_state(&expected);
    if rebuilt_state != expected_state {
        let mismatch = rebuilt_state
            .iter()
            .zip(&expected_state)
            .position(|(left, right)| left != right);
        bail!(
            "L4 state diverged at cutover height {}: rebuilt_orders={} snapshot_orders={} first_mismatch={mismatch:?}",
            validation.height,
            rebuilt_state.len(),
            expected_state.len()
        );
    }

    #[cfg(debug_assertions)]
    {
        rebuilt.assert_invariants();
        expected.assert_invariants();
    }
    println!(
        "[l4-book-smoke] ok base_height={} snapshot_height={} batches={} ops={} orders={} — initial snapshot + updates through cutover exactly matches new snapshot",
        base.height,
        validation.height,
        applied_batches,
        applied_ops,
        rebuilt.len()
    );
    Ok(())
}

async fn capture_snapshot(
    client: &L4SnapshotClient,
    coin: &str,
    primary: &mut L4Connection,
) -> Result<(AuthoritativeL4Snapshot, Vec<L4Updates>)> {
    let request = client.fetch_l4_snapshot_observation(coin, CancellationToken::new());
    tokio::pin!(request);
    let mut buffered = Vec::new();
    loop {
        tokio::select! {
            observation = &mut request => {
                let observation = observation?;
                println!(
                    "[l4-book-smoke] request={} authority={:?} time_source={:?} provider_operations={}",
                    observation.request_id,
                    observation.snapshot.authority,
                    observation.snapshot.exchange_time_source,
                    observation.provider_operations,
                );
                return Ok((observation.snapshot, buffered));
            }
            event = primary.next() => match event.ok_or_else(|| anyhow!("primary stream closed"))? {
                L4Event::Connected => println!("[l4-book-smoke] primary connected"),
                L4Event::Disconnected => bail!("primary stream disconnected during snapshot"),
                L4Event::Message(DwellirIncoming::L4Book(L4Message::Updates(update))) => {
                    push_bounded(&mut buffered, update)?;
                }
                L4Event::Message(DwellirIncoming::Error(error)) => {
                    bail!("provider error on primary stream: {error}");
                }
                _ => {}
            }
        }
    }
}

async fn next_update(primary: &mut L4Connection) -> Result<L4Updates> {
    loop {
        match primary
            .next()
            .await
            .ok_or_else(|| anyhow!("primary stream closed"))?
        {
            L4Event::Message(DwellirIncoming::L4Book(L4Message::Updates(update))) => {
                return Ok(update);
            }
            L4Event::Disconnected => bail!("primary stream disconnected during validation"),
            L4Event::Message(DwellirIncoming::Error(error)) => {
                bail!("provider error on primary stream: {error}");
            }
            _ => {}
        }
    }
}

fn push_bounded(updates: &mut Vec<L4Updates>, update: L4Updates) -> Result<()> {
    ensure!(
        updates.len() < MAX_BUFFERED_UPDATES,
        "bounded update buffer exhausted"
    );
    updates.push(update);
    Ok(())
}

fn assert_monotonic_heights(updates: &[L4Updates]) -> Result<()> {
    for pair in updates.windows(2) {
        ensure!(
            pair[0].height <= pair[1].height,
            "primary updates reordered: {} followed by {}",
            pair[0].height,
            pair[1].height
        );
    }
    Ok(())
}

fn book_from_snapshot(snapshot: &AuthoritativeL4Snapshot, scales: Scales) -> Result<OrderBook> {
    let orders = snapshot
        .orders
        .iter()
        .map(|order| convert_order(order, scales, None))
        .collect::<Result<Vec<_>>>()?;
    let mut book = OrderBook::with_capacity(orders.len());
    book.apply_snapshot(orders)?;
    Ok(book)
}

fn apply_update(
    book: &mut OrderBook,
    update: &L4Updates,
    coin: &str,
    scales: Scales,
) -> Result<usize> {
    let mut removed = HashSet::new();
    let mut applied = 0usize;
    for diff in &update.book_diffs {
        ensure!(diff.coin == coin, "coin mismatch in diff: {}", diff.coin);
        let op = operation_for_diff(book, diff, scales, &mut removed)?;
        if let Some(op) = op {
            book.apply_op(op)?;
            applied += 1;
        }
    }
    Ok(applied)
}

fn operation_for_diff(
    book: &mut OrderBook,
    diff: &L4BookDiff,
    scales: Scales,
    removed: &mut HashSet<u64>,
) -> Result<Option<BookOp>> {
    match &diff.raw_book_diff {
        RawBookDiff::New { sz } => {
            let order = diff
                .order
                .as_ref()
                .with_context(|| format!("new diff {} lacks joined order", diff.oid))?;
            Ok(Some(BookOp::Add(convert_order(order, scales, Some(*sz))?)))
        }
        RawBookDiff::Update { orig_sz, new_sz } => {
            let current = *book
                .get(diff.oid)
                .with_context(|| format!("update references missing order {}", diff.oid))?;
            let original_qty = fixed(*orig_sz, scales.qty_digits, "original quantity")?;
            ensure!(
                current.qty == original_qty,
                "order {} original quantity mismatch: book={} diff={}",
                diff.oid,
                current.qty,
                original_qty
            );
            size_operation(diff.oid, *new_sz, scales, removed, false)
        }
        RawBookDiff::Modified { sz } => {
            let current = *book
                .get(diff.oid)
                .with_context(|| format!("modified diff references missing order {}", diff.oid))?;
            let new_qty = fixed(*sz, scales.qty_digits, "modified quantity")?;
            if new_qty == 0 {
                removed.insert(diff.oid);
                return Ok(Some(BookOp::Remove(diff.oid)));
            }
            let new_price = fixed(diff.px, scales.price_digits, "modified price")?;
            if new_price == current.price {
                return Ok(Some(BookOp::AmendSize {
                    id: diff.oid,
                    new_qty,
                }));
            }

            // `l4-book` deliberately has no price-amend operation. A venue
            // price move loses its old queue position, so remove/reinsert is
            // the faithful representation.
            book.remove(diff.oid)?;
            Ok(Some(BookOp::Add(Order {
                price: new_price,
                qty: new_qty,
                ..current
            })))
        }
        RawBookDiff::Remove => {
            if removed.insert(diff.oid) {
                Ok(Some(BookOp::Remove(diff.oid)))
            } else {
                // Dwellir may emit both a zero-size fill and a remove for the
                // same OID in one batch. The first operation already removed it.
                Ok(None)
            }
        }
    }
}

fn size_operation(
    oid: u64,
    size: Decimal,
    scales: Scales,
    removed: &mut HashSet<u64>,
    amend: bool,
) -> Result<Option<BookOp>> {
    let new_qty = fixed(size, scales.qty_digits, "quantity")?;
    if new_qty == 0 {
        removed.insert(oid);
        Ok(Some(BookOp::Remove(oid)))
    } else if amend {
        Ok(Some(BookOp::AmendSize { id: oid, new_qty }))
    } else {
        Ok(Some(BookOp::UpdateSize { id: oid, new_qty }))
    }
}

fn convert_order(order: &L4Order, scales: Scales, size: Option<Decimal>) -> Result<Order> {
    let user = order
        .user
        .with_context(|| format!("order {} has no provider wallet", order.oid))?;
    let mut wallet = [0u8; 20];
    wallet.copy_from_slice(user.as_slice());
    Ok(Order {
        id: order.oid,
        wallet: WalletId(wallet),
        side: match order.side {
            HyperSide::Bid => Side::Bid,
            HyperSide::Ask => Side::Ask,
        },
        price: fixed(order.price, scales.price_digits, "price")?,
        qty: fixed(size.unwrap_or(order.size), scales.qty_digits, "quantity")?,
        ts: order
            .timestamp_ms
            .with_context(|| format!("order {} has no provider timestamp", order.oid))?,
    })
}

fn fixed(value: Decimal, digits: u32, field: &str) -> Result<u64> {
    l4_book::dwellir::parse_fixed(&value.to_string(), digits)
        .with_context(|| format!("invalid {field} {value}"))
}

fn complete_state(book: &OrderBook) -> Vec<Order> {
    let mut orders = Vec::with_capacity(book.len());
    for side in [Side::Bid, Side::Ask] {
        for (price, _, _) in book.depth(side) {
            orders.extend(book.orders_at(side, price).copied());
        }
    }
    orders
}

fn belongs_to_cutover(base_height: u64, snapshot_height: u64, update_height: u64) -> bool {
    // A snapshot is the complete post-state at its reported height, so its
    // same-height update must be replayed when rebuilding toward it. Conversely,
    // same-height updates must not be applied after installing that snapshot.
    update_height > base_height && update_height <= snapshot_height
}

fn scales_from_env() -> Result<Scales> {
    Ok(Scales {
        price_digits: env_digits("L4_PRICE_DIGITS", Scales::BTC_DEFAULT.price_digits)?,
        qty_digits: env_digits("L4_QTY_DIGITS", Scales::BTC_DEFAULT.qty_digits)?,
    })
}

fn env_digits(name: &str, default: u32) -> Result<u32> {
    env::var(name)
        .map(|value| value.parse().with_context(|| format!("invalid {name}")))
        .unwrap_or(Ok(default))
}

#[cfg(test)]
mod tests {
    use super::belongs_to_cutover;

    #[test]
    fn cutover_includes_validation_height_but_excludes_both_outer_ranges() {
        assert!(!belongs_to_cutover(100, 105, 100));
        assert!(belongs_to_cutover(100, 105, 101));
        assert!(belongs_to_cutover(100, 105, 105));
        assert!(!belongs_to_cutover(100, 105, 106));
    }
}
