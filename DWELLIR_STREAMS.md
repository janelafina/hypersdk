# Dwellir WebSocket Streams

`hypersdk::hypercore::dwellir` exposes Dwellir's Hyperliquid WebSocket feed for
real-time trades and L4 order book data. Both streams use the same reconnecting
connection type and message envelope.

## Setup

Configure a Dwellir dedicated node and API key:

```bash
export DWELLIR_NODE_HOST="dedicated-hyperliquid-...n.dwellir.com"
export DWELLIR_API_KEY="..."
```

Then create a connection:

```rust
use futures::StreamExt;
use hypersdk::hypercore::dwellir::{self, DwellirIncoming, DwellirSubscription, DwellirWsEvent};

let mut ws = dwellir::ws_from_env()?;
ws.subscribe(DwellirSubscription::Trades {
    coin: "BTC".into(),
    user: None,
});

while let Some(event) = ws.next().await {
    match event {
        DwellirWsEvent::Connected => {}
        DwellirWsEvent::Disconnected => {}
        DwellirWsEvent::Message(DwellirIncoming::Trades(trades)) => {
            for trade in trades {
                println!("{} {}@{}", trade.side, trade.sz, trade.px);
            }
        }
        _ => {}
    }
}
```

`DwellirWsConnection`, `L4Connection`, and `Config::ws_connection()` are aliases
or constructors for this same WebSocket client. The client reconnects with
exponential backoff and replays active subscriptions after reconnecting.

## Subscriptions

Subscriptions are represented by `DwellirSubscription`:

```rust
pub enum DwellirSubscription {
    L4Book { coin: String },
    Trades { coin: String, user: Option<Address> },
}
```

Use `Trades { user: None }` for all real-time executions in a market, or pass a
wallet address to receive only executions involving that user. Use
`L4Book { coin }` for the full per-order book.

Incoming WebSocket messages are represented by `DwellirIncoming`:

```rust
pub enum DwellirIncoming {
    SubscriptionResponse(DwellirOutgoing),
    Trades(Vec<DwellirTrade>),
    L4Book(L4Message),
}
```

## Trade Stream

The trade stream returns `DwellirIncoming::Trades(Vec<DwellirTrade>)`.
`DwellirTrade` is an alias for the SDK's native `hypercore::types::Trade`:

```rust
pub struct Trade {
    pub coin: String,
    pub side: Side,
    pub px: Decimal,
    pub sz: Decimal,
    pub time: u64,
    pub hash: String,
    pub tid: u64,
    pub users: [Address; 2],
    pub liquidation: Option<Liquidation>,
}
```

`side` is from the taker's perspective (`Side::Bid` for buy, `Side::Ask` for
sell). Helper methods on `Trade` include `notional()`, `is_buy()`, `is_sell()`,
`is_liquidation()`, `taker_address()`, and `maker_address()`.

Example:

```rust
ws.subscribe(DwellirSubscription::Trades {
    coin: "BTC".into(),
    user: None,
});
```

See `examples/hypercore/dwellir_trades.rs` for a runnable example.

## L4 Book Stream

The L4 book stream returns `DwellirIncoming::L4Book(L4Message)`.

```rust
pub enum L4Message {
    Snapshot(L4Snapshot),
    Updates(L4Updates),
}
```

`L4Message::Snapshot` contains a full per-order book:

```rust
pub struct L4Snapshot {
    pub coin: String,
    pub height: u64,
    pub time: Option<u64>,
    pub levels: [Vec<L4Order>; 2],
    pub metadata: L4MessageMetadata,
}
```

`levels[0]` is bids and `levels[1]` is asks. The SDK also provides
`bids()` and `asks()` accessors. `time` is optional only because legacy
Dwellir WebSocket nodes omit it. The strict snapshot API never substitutes a
receipt time and returns `L4SnapshotError::MissingExchangeTimestamp` instead.

`L4Message::Updates` contains incremental changes for a block:

```rust
pub struct L4Updates {
    pub time: u64,
    pub height: u64,
    pub order_statuses: Vec<L4OrderStatus>,
    pub book_diffs: Vec<L4BookDiff>,
    pub metadata: L4MessageMetadata,
}
```

`order_statuses` reports order state transitions such as open, filled, or
canceled. `book_diffs` reports mutations to resting order size or presence.

Individual L4 orders are represented by `L4Order`:

```rust
pub struct L4Order {
    pub user: Option<Address>,
    pub coin: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub oid: u64,
    pub timestamp_ms: Option<u64>,
    pub trigger_condition: Option<String>,
    pub is_trigger: bool,
    pub trigger_px: Option<Decimal>,
    pub is_position_tpsl: bool,
    pub reduce_only: bool,
    pub order_type: Option<String>,
    pub tif: Option<String>,
    pub cloid: Option<String>,
    pub original_size: Option<Decimal>,
    pub children: Vec<L4Order>,
    pub extra: Map<String, Value>,
}
```

`user` is optional because Dwellir's provider-RPC snapshot schema does not
always include an owner; the SDK never invents one. `extra` retains newly added
provider fields rather than silently dropping them. `limit_px()` and `sz()`
accessors are available for callers migrating from the previous field names.

Book diffs are represented by:

```rust
pub struct L4BookDiff {
    pub user: Address,
    pub oid: u64,
    pub px: Decimal,
    pub coin: String,
    pub raw_book_diff: RawBookDiff,
    pub order: Option<L4Order>,
}

pub enum RawBookDiff {
    New { sz: Decimal },
    Update { orig_sz: Decimal, new_sz: Decimal },
    Modified { sz: Decimal },
    Remove,
}
```

For a `New` diff, `order` is joined by OID from the same batch's order-status
record. This preserves placement timestamp, CLOID, side, and all other order
fields. If the provider does not supply a joinable full order, it remains
`None`; `L4BookRecorder` rejects it rather than reconstructing missing fields.

Example:

```rust
ws.subscribe(DwellirSubscription::L4Book { coin: "BTC".into() });
```

See `examples/hypercore/dwellir_l4.rs` for a runnable example.

## Authoritative snapshots

`Config::fetch_l4_snapshot` opens a dedicated short-lived L4 subscription for
the requested coin. If its snapshot has no `time`, the SDK calls Dwellir
`GetBlock` for exactly the reported snapshot height and uses that block's
consensus timestamp. Both values therefore come from provider/node protocol;
receipt time and later update timestamps are never substituted. These requests
do not share, reset, pause, reconnect, or buffer the primary WebSocket stream.

```rust
let config = dwellir::Config::from_env()?;
let snapshot = config.fetch_l4_snapshot("BTC").await?;
assert_eq!(snapshot.authority, SnapshotAuthority::FreshSubscription);
assert_eq!(
    snapshot.exchange_time_source,
    SnapshotExchangeTimeSource::ExactHeightBlockRpc,
);
println!(
    "{} @ height {} time {}: {} orders",
    snapshot.coin,
    snapshot.height,
    snapshot.exchange_time_ms,
    snapshot.orders.len(),
);
```

For full request provenance and cancellation, use the shared snapshot client:

```rust
use tokio_util::sync::CancellationToken;

let client = config.l4_snapshot_client();
let observation = client
    .fetch_l4_snapshot_observation("BTC", CancellationToken::new())
    .await?;
println!("request={}", observation.request_id);
```

The client permits two concurrent requests by default, performs exactly one
attempt, and has a 120-second timeout. A legacy snapshot requires two provider
operations (subscribe plus exact-height block lookup), reported in observation
metadata. Builders can change concurrency and timeout. Cancellation or timeout
drops the isolated request/channel. There is no local-book fallback.

`fetch_l4_snapshot_via_provider_rpc` is an explicit alternative using Dwellir's
all-market `GetOrderBookSnapshot` gRPC response and a 512 MiB client ceiling.
It is useful on premium deployments configured for very large responses. The
default coin-specific path avoids that bandwidth and server-size requirement.

### Ordering semantics

- A snapshot is complete state at its reported height and is returned only
  after the full order set decodes and validates.
- Only updates with a strictly greater height may be applied afterward.
- A lower snapshot is stale. A same-height snapshot replaces that height's
  state, making repeated same-height snapshots idempotent.
- Bids are returned first by descending price, asks second by ascending price;
  ties use placement timestamp and OID.
- `L4BookRecorder` atomically enforces these rules and treats missing `New`
  order joins or inconsistent sizes as reconstruction/gap errors.

### Runtime capability discovery

`l4_capabilities()` describes what the SDK implementation supports.
`discover_l4_capabilities(coin).await` actively requests and validates a real
provider snapshot, returning all capability flags false plus a failure message
when the endpoint is not entitled, exceeds provider limits, lacks timestamp or
height, or cannot decode the schema. On success it also returns the observation
so a runner can reuse it as the initial book.

### Correlation metadata

Snapshots and updates retain provider connection/subscription IDs, sequence,
request ID, schema/protocol version, and checksum when present. The SDK creates
only a reconnect-scoped WebSocket connection ID when the provider omits one;
provider sequence and request IDs are never synthesized. Receipt time is
available only as `SnapshotObservation::receipt_time_ms` and is never used as
the authoritative exchange timestamp.

Run the live interleaving smoke test with:

```bash
cargo run --example dwellir_l4_snapshot_smoke -- BTC
```

It records the hot WebSocket while isolated snapshots are in flight, applies
only updates newer than each snapshot, and verifies the rebuilt resting book
against a second authoritative snapshot without perturbing the primary stream.
