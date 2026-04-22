//! Types for Dwellir Hyperliquid streams (L4 book + fills).

use alloy::primitives::{Address, B128};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};

use crate::hypercore::types::{Liquidation, Side};

/// Outgoing WebSocket messages for the Dwellir L4 feed.
///
/// Wire format matches Hyperliquid's native WebSocket subscribe/unsubscribe
/// envelope (`{"method": "subscribe", "subscription": {...}}`), but the set
/// of allowed `subscription` values is Dwellir-specific.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum DwellirOutgoing {
    Subscribe { subscription: DwellirSubscription },
    Unsubscribe { subscription: DwellirSubscription },
}

/// Dwellir-specific subscription channels.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, derive_more::Display)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DwellirSubscription {
    /// Full L4 order book with individual-order visibility.
    #[display("l4Book({coin})")]
    L4Book { coin: String },
}

/// Incoming WebSocket messages from the Dwellir L4 feed.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "channel", content = "data", rename_all = "camelCase")]
pub enum DwellirIncoming {
    /// Echo of a subscribe/unsubscribe request.
    SubscriptionResponse(DwellirOutgoing),
    /// L4 book message (either a full snapshot or an incremental update batch).
    L4Book(L4Message),
}

/// Either a full snapshot or an incremental update batch for the L4 book.
#[derive(Debug, Clone, Deserialize)]
pub enum L4Message {
    Snapshot(L4Snapshot),
    Updates(L4Updates),
}

/// Full L4 order-book snapshot.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L4Snapshot {
    pub coin: String,
    /// Hyperliquid block height at which the snapshot was taken.
    pub height: u64,
    /// `[bids, asks]`.
    pub levels: [Vec<L4Order>; 2],
}

impl L4Snapshot {
    pub fn bids(&self) -> &[L4Order] {
        &self.levels[0]
    }
    pub fn asks(&self) -> &[L4Order] {
        &self.levels[1]
    }
}

/// Incremental L4 update batch.
///
/// Field names match Dwellir's wire format, which is snake_case for this
/// message (whereas the nested [`L4Order`] uses camelCase).
#[derive(Debug, Clone, Deserialize)]
pub struct L4Updates {
    /// Unix timestamp in milliseconds.
    pub time: u64,
    /// Hyperliquid block height.
    pub height: u64,
    /// Order status transitions that occurred in this block.
    #[serde(default)]
    pub order_statuses: Vec<L4OrderStatus>,
    /// Per-order book mutations that occurred in this block.
    #[serde(default)]
    pub book_diffs: Vec<L4BookDiff>,
}

/// Single order status transition (e.g. `open`, `filled`, `canceled`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L4OrderStatus {
    /// ISO-8601 timestamp with nanosecond precision as emitted by the node.
    pub time: String,
    pub user: Address,
    pub status: String,
    pub order: L4Order,
}

/// Individual order record as it appears on the L4 book.
///
/// Several string fields are typed as `Option` because the server sometimes
/// emits `null` for them (in practice it happens deep inside large snapshots
/// for fields like `triggerCondition`, `tif`, `orderType`, and `triggerPx`).
/// Treat `None` as "not set / not applicable".
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L4Order {
    /// Owner wallet. `None` inside `order_statuses.order` — the outer
    /// `L4OrderStatus.user` is the authoritative owner in that case.
    #[serde(default)]
    pub user: Option<Address>,
    pub coin: String,
    pub side: Side,
    pub limit_px: Decimal,
    pub sz: Decimal,
    pub oid: u64,
    pub timestamp: u64,
    #[serde(default)]
    pub trigger_condition: Option<String>,
    pub is_trigger: bool,
    #[serde(default)]
    pub trigger_px: Option<Decimal>,
    pub is_position_tpsl: bool,
    pub reduce_only: bool,
    #[serde(default)]
    pub order_type: Option<String>,
    #[serde(default)]
    pub tif: Option<String>,
    #[serde(default)]
    pub cloid: Option<B128>,
}

/// Mutation of a specific order's footprint on the book.
#[derive(Debug, Clone, Deserialize)]
pub struct L4BookDiff {
    pub user: Address,
    pub oid: u64,
    pub px: Decimal,
    pub coin: String,
    pub raw_book_diff: RawBookDiff,
}

/// What happened to the order referenced by an [`L4BookDiff`].
#[derive(Debug, Clone)]
pub enum RawBookDiff {
    /// New resting order with the given size.
    New { sz: Decimal },
    /// Size decreased from `orig_sz` to `new_sz` (typically a partial fill).
    Update { orig_sz: Decimal, new_sz: Decimal },
    /// Size amended to `sz` (e.g. order modify).
    Modified { sz: Decimal },
    /// Order removed (full fill, cancel, or expiry).
    Remove,
}

impl<'de> Deserialize<'de> for RawBookDiff {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(deserializer)?;
        match v {
            serde_json::Value::String(s) if s == "remove" => Ok(Self::Remove),
            serde_json::Value::Object(m) if m.len() == 1 => {
                let (k, body) = m.into_iter().next().unwrap();
                match k.as_str() {
                    "new" => {
                        #[derive(Deserialize)]
                        struct S {
                            sz: Decimal,
                        }
                        let s: S = serde_json::from_value(body).map_err(D::Error::custom)?;
                        Ok(Self::New { sz: s.sz })
                    }
                    "update" => {
                        #[derive(Deserialize)]
                        #[serde(rename_all = "camelCase")]
                        struct S {
                            orig_sz: Decimal,
                            new_sz: Decimal,
                        }
                        let s: S = serde_json::from_value(body).map_err(D::Error::custom)?;
                        Ok(Self::Update {
                            orig_sz: s.orig_sz,
                            new_sz: s.new_sz,
                        })
                    }
                    "modified" => {
                        #[derive(Deserialize)]
                        struct S {
                            sz: Decimal,
                        }
                        let s: S = serde_json::from_value(body).map_err(D::Error::custom)?;
                        Ok(Self::Modified { sz: s.sz })
                    }
                    other => Err(D::Error::custom(format!(
                        "unknown raw_book_diff variant: {other}"
                    ))),
                }
            }
            other => Err(D::Error::custom(format!(
                "invalid raw_book_diff payload: {other}"
            ))),
        }
    }
}

/// Block of fills emitted by Dwellir's `StreamFills` gRPC — one message per block.
///
/// This is the parsed Rust view of the JSON payload that Dwellir nests inside
/// `BlockFills.data` (the node's `node_fills` / `node_fills_by_block` shape).
#[derive(Debug, Clone, Deserialize)]
pub struct FillsBlock {
    /// Node-local time (ISO-8601, nanosecond precision).
    pub local_time: String,
    /// Consensus block time (ISO-8601).
    pub block_time: String,
    /// Block height.
    pub block_number: u64,
    /// One entry per fill in the block: `(user_address, fill)`.
    pub events: Vec<(Address, DwellirFill)>,
}

/// Effect of a fill on the user's inventory, derived from [`DwellirFill::dir`].
///
/// Per Hyperliquid's `node_fills` spec the `dir` field is one of `"Open Long"`,
/// `"Open Short"`, `"Close Long"`, `"Close Short"` for perps. Anything else
/// (spot `"Buy"`/`"Sell"`, position flips, etc.) is reported as [`Other`].
///
/// [`Other`]: FillInventoryEffect::Other
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillInventoryEffect {
    /// Fill increased inventory (`"Open Long"` or `"Open Short"`).
    Increase,
    /// Fill decreased inventory (`"Close Long"` or `"Close Short"`).
    Decrease,
    /// Any other `dir` value (e.g. spot `"Buy"`/`"Sell"`, position flip).
    Other,
}

/// Fill as reported by Dwellir's fills stream.
///
/// A superset of the native HL WebSocket [`Fill`](crate::hypercore::types::Fill):
/// includes `twap_id`, `builder`, and `builder_fee`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DwellirFill {
    pub coin: String,
    pub px: Decimal,
    pub sz: Decimal,
    pub side: Side,
    pub time: u64,
    pub start_position: Decimal,
    pub dir: String,
    pub closed_pnl: Decimal,
    pub hash: String,
    pub oid: u64,
    pub crossed: bool,
    pub fee: Decimal,
    pub tid: u64,
    pub cloid: Option<B128>,
    pub fee_token: String,
    #[serde(default)]
    pub twap_id: Option<u64>,
    #[serde(default)]
    pub liquidation: Option<Liquidation>,
    #[serde(default)]
    pub builder: Option<Address>,
    #[serde(default)]
    pub builder_fee: Option<Decimal>,
}

impl DwellirFill {
    /// Notional value of the fill (price × size).
    pub fn notional(&self) -> Decimal {
        self.px * self.sz
    }

    /// `true` if this fill was the taker side (crossed the spread).
    pub fn is_taker(&self) -> bool {
        self.crossed
    }

    /// `true` if this fill was the maker side (resting on the book).
    pub fn is_maker(&self) -> bool {
        !self.crossed
    }

    /// `true` if this fill was part of a forced liquidation.
    pub fn is_liquidation(&self) -> bool {
        self.liquidation.is_some()
    }

    /// Classify the fill's effect on the user's inventory based on [`dir`].
    ///
    /// `"Open Long"`/`"Open Short"` → [`Increase`], `"Close Long"`/`"Close Short"`
    /// → [`Decrease`], everything else → [`Other`].
    ///
    /// [`dir`]: Self::dir
    /// [`Increase`]: FillInventoryEffect::Increase
    /// [`Decrease`]: FillInventoryEffect::Decrease
    /// [`Other`]: FillInventoryEffect::Other
    pub fn inventory_effect(&self) -> FillInventoryEffect {
        match self.dir.as_str() {
            "Open Long" | "Open Short" => FillInventoryEffect::Increase,
            "Close Long" | "Close Short" => FillInventoryEffect::Decrease,
            _ => FillInventoryEffect::Other,
        }
    }

    /// `true` if this fill increased the user's inventory (opened a position).
    pub fn is_increasing_inventory(&self) -> bool {
        self.inventory_effect() == FillInventoryEffect::Increase
    }

    /// `true` if this fill decreased the user's inventory (closed a position).
    pub fn is_decreasing_inventory(&self) -> bool {
        self.inventory_effect() == FillInventoryEffect::Decrease
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_l4_snapshot() {
        let raw = r#"{
            "channel": "l4Book",
            "data": {
                "Snapshot": {
                    "coin": "BTC",
                    "height": 854890775,
                    "levels": [
                        [{
                            "user": "0xf9109ada2f73c62e9889b45453065f0d99260a2d",
                            "coin": "BTC",
                            "side": "B",
                            "limitPx": "90057",
                            "sz": "0.33289",
                            "oid": 289682065711,
                            "timestamp": 1767878782721,
                            "triggerCondition": "N/A",
                            "isTrigger": false,
                            "triggerPx": "0.0",
                            "isPositionTpsl": false,
                            "reduceOnly": false,
                            "orderType": "Limit",
                            "tif": "Alo",
                            "cloid": "0x4c4617dbd8b94d358285c5c6d5a43df3"
                        }],
                        []
                    ]
                }
            }
        }"#;
        let msg: DwellirIncoming = serde_json::from_str(raw).unwrap();
        let DwellirIncoming::L4Book(L4Message::Snapshot(snap)) = msg else {
            panic!("expected snapshot");
        };
        assert_eq!(snap.coin, "BTC");
        assert_eq!(snap.bids().len(), 1);
        assert_eq!(snap.asks().len(), 0);
    }

    #[test]
    fn parses_l4_updates_with_remove() {
        let raw = r#"{
            "channel": "l4Book",
            "data": {
                "Updates": {
                    "time": 1,
                    "height": 2,
                    "order_statuses": [],
                    "book_diffs": [{
                        "user": "0xbc927e87d072dfac3693846a83fa6922cc6c5f2a",
                        "oid": 1,
                        "px": "100.0",
                        "coin": "BTC",
                        "raw_book_diff": "remove"
                    }]
                }
            }
        }"#;
        let msg: DwellirIncoming = serde_json::from_str(raw).unwrap();
        let DwellirIncoming::L4Book(L4Message::Updates(up)) = msg else {
            panic!("expected updates");
        };
        assert!(matches!(up.book_diffs[0].raw_book_diff, RawBookDiff::Remove));
    }

    #[test]
    fn parses_update_diff() {
        let raw = r#"{
            "user": "0x97991003fd631e2923f40cab2a4fdc35e60dc807",
            "oid": 316542552323,
            "px": "84.371",
            "coin": "SOL",
            "raw_book_diff": { "update": { "origSz": "108.65", "newSz": "107.5" } }
        }"#;
        let diff: L4BookDiff = serde_json::from_str(raw).unwrap();
        match diff.raw_book_diff {
            RawBookDiff::Update { orig_sz, new_sz } => {
                assert_eq!(orig_sz.to_string(), "108.65");
                assert_eq!(new_sz.to_string(), "107.5");
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parses_fills_block() {
        let raw = r#"{
            "local_time": "2025-07-27T08:50:10.334741319",
            "block_time": "2025-07-27T08:50:10.273720809",
            "block_number": 676607012,
            "events": [
                ["0x7839e2f2c375dd2935193f2736167514efff9916", {
                    "coin": "BTC",
                    "px": "118136.0",
                    "sz": "0.00009",
                    "side": "B",
                    "time": 1753606210273,
                    "startPosition": "-1.41864",
                    "dir": "Close Short",
                    "closedPnl": "-0.003753",
                    "hash": "0xe7822040155eaa2e737e042854342401120052bbf063906ce8c8f3babe853a79",
                    "oid": 121670079265,
                    "crossed": false,
                    "fee": "-0.000212",
                    "tid": 161270588369408,
                    "cloid": "0x09367b9f8541c581f95b02aaf05f1508",
                    "feeToken": "USDC",
                    "twapId": null,
                    "liquidation": null,
                    "builder": "0x49ae63056b3a0be0b166813ee687309ab653c07c",
                    "builderFee": "0.005528"
                }]
            ]
        }"#;
        let block: FillsBlock = serde_json::from_str(raw).unwrap();
        assert_eq!(block.block_number, 676607012);
        assert_eq!(block.events.len(), 1);
        let (_, fill) = &block.events[0];
        assert_eq!(fill.coin, "BTC");
        assert!(fill.is_maker());
        assert!(fill.builder.is_some());
        assert_eq!(fill.inventory_effect(), FillInventoryEffect::Decrease);
        assert!(fill.is_decreasing_inventory());
        assert!(!fill.is_increasing_inventory());
    }

    #[test]
    fn classifies_inventory_effect() {
        let mut fill = DwellirFill {
            coin: "BTC".into(),
            px: Decimal::ONE,
            sz: Decimal::ONE,
            side: Side::Bid,
            time: 0,
            start_position: Decimal::ZERO,
            dir: String::new(),
            closed_pnl: Decimal::ZERO,
            hash: String::new(),
            oid: 0,
            crossed: false,
            fee: Decimal::ZERO,
            tid: 0,
            cloid: None,
            fee_token: "USDC".into(),
            twap_id: None,
            liquidation: None,
            builder: None,
            builder_fee: None,
        };

        for dir in ["Open Long", "Open Short"] {
            fill.dir = dir.into();
            assert_eq!(fill.inventory_effect(), FillInventoryEffect::Increase);
            assert!(fill.is_increasing_inventory());
            assert!(!fill.is_decreasing_inventory());
        }

        for dir in ["Close Long", "Close Short"] {
            fill.dir = dir.into();
            assert_eq!(fill.inventory_effect(), FillInventoryEffect::Decrease);
            assert!(fill.is_decreasing_inventory());
            assert!(!fill.is_increasing_inventory());
        }

        for dir in ["Buy", "Sell", "Long > Short", ""] {
            fill.dir = dir.into();
            assert_eq!(fill.inventory_effect(), FillInventoryEffect::Other);
            assert!(!fill.is_increasing_inventory());
            assert!(!fill.is_decreasing_inventory());
        }
    }
}
