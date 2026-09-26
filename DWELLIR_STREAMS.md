# Dwellir Streams

`hypersdk::hypercore::dwellir` exposes Dwellir's Hyperliquid WebSocket feed for
real-time trades and L4 order book data. Both streams use the same reconnecting
connection type and message envelope. Aggregated L2 book diffs are delivered
over gRPC instead; see [L2 Book Diff Stream (gRPC)](#l2-book-diff-stream-grpc).

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

## L2 Book Diff Stream (gRPC)

Dwellir's v3 `MarketStreaming` service exposes
`hyperliquid_l1_gateway.v3.MarketStreaming/StreamL2BookDiff`: aggregated (L2)
price-level changes for 1-20 coins on one server-streaming RPC. It uses the
same dedicated node gRPC endpoint and `x-api-key` metadata as the fills
stream (TLS on `https://{DWELLIR_NODE_HOST}:443`).

### Setup

```bash
export DWELLIR_NODE_HOST="dedicated-hyperliquid-...n.dwellir.com"
export DWELLIR_API_KEY="..."
# or, without a node host: DWELLIR_GRPC_ENDPOINT="https://...:443" + DWELLIR_API_KEY
```

```rust
use futures::StreamExt;
use hypersdk::hypercore::dwellir::{self, L2BookDiffEvent, L2BookDiffRequest, L2BookSet};

let request = L2BookDiffRequest::new(["BTC", "ETH"]).n_levels(50);
let mut conn = dwellir::l2_book_diff_from_env(request)?;
// or: dwellir::Config::from_env()?.l2_book_diff_connection(request)?
let mut books = L2BookSet::from_request(conn.request());

while let Some(event) = conn.next().await {
    match event {
        L2BookDiffEvent::Connected => {}
        L2BookDiffEvent::Disconnected => {}
        L2BookDiffEvent::Message(update) => match books.apply(&update) {
            Ok(_) => {
                let btc = books.get("BTC").unwrap();
                println!("{:?} / {:?}", btc.best_bid(), btc.best_ask());
            }
            // Lost frames: resubscribe; the fresh snapshots rebuild the books.
            Err(err) if err.requires_resync() => conn.reconnect(),
            Err(err) => eprintln!("{err}"),
        },
        L2BookDiffEvent::Error(err) => {
            eprintln!("fatal: {err}");
            break; // terminal: the stream ends after this event
        }
    }
}
```

### Request parameters

`L2BookDiffRequest` is validated client-side before any connection is made
(`L2BookDiffRequestError` on failure):

| Field        | Rule                                                                 |
|--------------|----------------------------------------------------------------------|
| `coins`      | Required, 1-20 distinct case-sensitive names. Duplicates are removed. |
| `n_levels`   | `None` = server default (20); `Some(0)` = full depth (endpoint must support it, otherwise `FAILED_PRECONDITION`); otherwise 1-100. |
| `n_sig_figs` | 2, 3, 4 or 5.                                                         |
| `mantissa`   | Only with `n_sig_figs == 5`; 2 or 5.                                  |

Builder helpers: `.n_levels(n)`, `.full_depth()`, `.n_sig_figs(n)`,
`.mantissa(m)`.

### Events

`L2BookDiffConnection` (and its detached `L2BookDiffConnectionStream`) yield
`L2BookDiffEvent`:

- `Connected` — stream opened; fresh `snapshot: true` entries follow for every
  coin.
- `Disconnected` — stream closed; a reconnect (with exponential backoff) is
  already underway. `ABORTED` (server frame loss), `DEADLINE_EXCEEDED` (slow
  consumer), `UNAVAILABLE` and transport errors are retried.
- `Message(L2BookDiffUpdate)` — one coalesce window:
  `{ time, block_number, diffs: Vec<L2CoinDiff> }`, where
  `L2CoinDiff { coin, seq, prev_seq, bids, asks, snapshot }` and levels are
  the SDK's `BookLevel { px: Decimal, sz: Decimal, n }` (alias `L2DiffLevel`).
  Decimals are parsed strictly when the frame is received.
- `Error(L2BookDiffStreamError)` — terminal. `INVALID_ARGUMENT`,
  `UNAUTHENTICATED`, `PERMISSION_DENIED`, `FAILED_PRECONDITION` and
  `UNIMPLEMENTED` cannot be fixed by retrying, so they are reported once and
  the stream ends instead of reconnecting in a loop.

A frame that fails typed conversion (e.g. an unparsable decimal) is logged
and the connection resubscribes, because silently skipping it would break the
per-coin sequence chain. Unchanged coins are omitted from frames, windows
with no change produce no frame, and there are no heartbeats: a quiet stream
is healthy unless it is closed with an error status.

### Sequencing and recovery

- The stream opens with one `snapshot: true` entry per coin (`seq: 1`,
  `prev_seq: 0`); later entries carry only changed levels. A level with
  `sz == 0` (e.g. `"0.0"`) removes that price.
- Sequence numbers are per coin. Every entry's `prev_seq` must equal the last
  accepted `seq` for that coin, and `seq == prev_seq + 1`. Anything else is
  loss.
- There is no resume cursor: every reconnect re-opens with fresh snapshots.
  To recover from a detected gap call `L2BookDiffConnection::reconnect()` (or
  `L2BookDiffHandle::reconnect()` after `split()`).

### Recorders

`L2BookRecorder` holds one coin's book (bids descending, asks ascending) and
exposes `bids()`, `asks()`, `best_bid()`, `best_ask()`, `seq()`,
`block_number()` and `time_ms()`. `L2BookSet` holds one recorder per
subscribed coin and routes each frame.

- A `snapshot: true` entry always replaces the coin's book and adopts its
  `seq`, whatever the previous `seq` was. Reconnects and server-initiated
  resets therefore need no manual `reset()`.
- A non-snapshot entry before any snapshot, a `prev_seq` mismatch, or a bad
  `seq` step returns `L2ReconstructionError` (`MissingBaseSnapshot`,
  `SequenceGap`, `InvalidSeqStep`; `requires_resync()` is `true`) and leaves
  the book unchanged.
- `L2BookSet::apply` is atomic per frame: all entries are validated before any
  are applied. Entries for coins the set does not track return `UnknownCoin`;
  two entries for one coin return `DuplicateCoin`.
- Removing a price that is not in the book is treated as a no-op; duplicate
  prices inside one snapshot return `DuplicateLevel`.
- A recorder's `block_number()` only advances when its coin changes;
  `L2BookSet::block_number()` tracks the latest frame.

### Unified book subscription

`BookConnection` lets downstream code choose the book feed at runtime:

```rust
use hypersdk::hypercore::dwellir::{self, BookEvent, BookMessage, BookSubscription, L2BookDiffRequest};

let sub = if use_l4 {
    BookSubscription::l4("BTC") // WebSocket L4Book
} else {
    BookSubscription::l2_diff(L2BookDiffRequest::new(["BTC", "ETH"])) // gRPC diffs
};
let mut books = dwellir::book_from_env(sub)?; // or Config::book_connection(sub)?

while let Some(event) = books.next().await {
    match event {
        BookEvent::Message(BookMessage::L4(msg)) => { /* L4Message */ }
        BookEvent::Message(BookMessage::L2(update)) => { /* L2BookDiffUpdate */ }
        BookEvent::Error(err) if err.is_terminal() => break,
        _ => {}
    }
}
```

Payloads are not normalized: L4 carries individual orders and L2 carries
aggregated levels, so `BookMessage` keeps each in its typed form. Recorders
stay separate (`L4BookRecorder` needs authoritative snapshots with exchange
time; use `L2BookSet` for L2). `BookConnection::reconnect()` forces a resync
on the L2 transport and returns `false` for L4. L4 provider errors are
reported as non-terminal `BookError::L4Provider`.

Run the example (add `--l4` to use the L4 feed through the same interface):

```bash
cargo run --example dwellir_l2_book_diff -- BTC ETH
cargo run --example dwellir_l2_book_diff -- --l4 BTC
```

## BBO Stream (gRPC)

Dwellir's v3 `MarketStreaming` service also exposes
`hyperliquid_l1_gateway.v3.MarketStreaming/StreamBbo`: the best bid and ask of
1-20 coins (main-dex and builder-dex names such as `xyz:XYZ100`) on one
server-streaming RPC, with the same endpoint and `x-api-key` metadata as the
L2 diff stream. It opens with the current top of book of every coin, then sends
one `BboUpdate` per coin whenever that coin's top level (price, size or order
count) changes. Cross-coin order is unspecified; a frame may repeat a value
after the server recovers from subscriber lag; `ABORTED` means reconnect for
fresh opening values.

```rust,ignore
use hypersdk::hypercore::dwellir::{Bbo, BboRequest, stream_bbo};

let mut stream = stream_bbo(&endpoint, Some(&api_key), &BboRequest::new(["BTC", "ETH"]))
    .await?
    .into_inner();
while let Some(update) = stream.message().await? {
    let bbo = Bbo::try_from(update)?; // coin, time, block_number, bid, ask
}
```

`stream_bbo` opens one subscription; the caller owns reconnection. The field
numbers of `BboRequest`/`BboUpdate` were pinned from live frames (see
`src/hypercore/dwellir/bbo.rs`). See `examples/hypercore/dwellir_bbo.rs`.
