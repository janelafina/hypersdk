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
    pub levels: [Vec<L4Order>; 2],
}
```

`levels[0]` is bids and `levels[1]` is asks. The SDK also provides
`bids()` and `asks()` accessors.

`L4Message::Updates` contains incremental changes for a block:

```rust
pub struct L4Updates {
    pub time: u64,
    pub height: u64,
    pub order_statuses: Vec<L4OrderStatus>,
    pub book_diffs: Vec<L4BookDiff>,
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
    pub limit_px: Decimal,
    pub sz: Decimal,
    pub oid: u64,
    pub timestamp: u64,
    pub trigger_condition: Option<String>,
    pub is_trigger: bool,
    pub trigger_px: Option<Decimal>,
    pub is_position_tpsl: bool,
    pub reduce_only: bool,
    pub order_type: Option<String>,
    pub tif: Option<String>,
    pub cloid: Option<B128>,
}
```

Book diffs are represented by:

```rust
pub struct L4BookDiff {
    pub user: Address,
    pub oid: u64,
    pub px: Decimal,
    pub coin: String,
    pub raw_book_diff: RawBookDiff,
}

pub enum RawBookDiff {
    New { sz: Decimal },
    Update { orig_sz: Decimal, new_sz: Decimal },
    Modified { sz: Decimal },
    Remove,
}
```

Example:

```rust
ws.subscribe(DwellirSubscription::L4Book { coin: "BTC".into() });
```

See `examples/hypercore/dwellir_l4.rs` for a runnable example.
