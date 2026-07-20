//! Strict, isolated authoritative L4 snapshot acquisition.

use std::{
    cmp::Ordering,
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, NaiveDateTime};
use serde_json::{Map, Value};
use tokio::{sync::Semaphore, time::timeout};
use tokio_util::sync::CancellationToken;
use tonic::Code;
use url::Url;

use super::{
    grpc::{SnapshotRpcError, get_block_at_height, get_latest_orderbook_snapshot},
    types::{L4MessageMetadata, L4Order, L4Snapshot, L4Updates},
    ws::{OneShotSnapshotError, fetch_first_l4_snapshot},
};
use crate::hypercore::types::Side;

const DEFAULT_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_MAX_CONCURRENT_SNAPSHOTS: usize = 2;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Why a snapshot is authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotAuthority {
    /// Dwellir's Hyperliquid L1 Gateway `GetOrderBookSnapshot` RPC.
    ProviderRpc,
    /// First complete snapshot from a new, dedicated L4 subscription.
    FreshSubscription,
}

/// Exact provider field/RPC from which `exchange_time_ms` was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotExchangeTimeSource {
    /// Timestamp included directly in a WebSocket snapshot payload.
    SnapshotPayload,
    /// Timestamp included in `GetOrderBookSnapshot` provider data.
    ProviderSnapshotRpc,
    /// Consensus block timestamp returned by `GetBlock` for exactly the
    /// snapshot's reported height.
    ExactHeightBlockRpc,
}

/// Complete L4 state at an exact provider height and exchange/node time.
#[derive(Debug, Clone)]
pub struct AuthoritativeL4Snapshot {
    pub coin: String,
    pub height: u64,
    pub exchange_time_ms: u64,
    pub exchange_time_source: SnapshotExchangeTimeSource,
    /// Deterministically ordered: bids by descending price, then asks by
    /// ascending price; ties use placement timestamp and OID.
    pub orders: Vec<L4Order>,
    pub authority: SnapshotAuthority,
    /// Provider correlation/schema fields retained from the response.
    pub metadata: L4MessageMetadata,
}

impl AuthoritativeL4Snapshot {
    /// Only strictly newer update heights may be applied after this snapshot.
    /// Same-height updates are already represented by the complete snapshot.
    #[must_use]
    pub fn accepts_update(&self, update: &L4Updates) -> bool {
        update.height > self.height
    }

    /// A snapshot below the consumer's current height is stale. A same-height
    /// snapshot is not stale and deterministically replaces that height's
    /// state (making repeated same-height snapshots idempotent).
    #[must_use]
    pub fn is_stale_at(&self, current_height: u64) -> bool {
        self.height < current_height
    }
}

/// Result of applying a complete snapshot or update batch to [`L4BookRecorder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L4ApplyOutcome {
    Applied,
    /// The message height was older than the recorder's state, or was a
    /// same-height update already included in a complete snapshot/batch.
    IgnoredStaleOrSameHeight,
}

/// Strict reconstruction failure, normally indicating a gap or incomplete
/// `new`-order join rather than something callers should guess around.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum L4ReconstructionError {
    #[error("book has no authoritative base snapshot")]
    MissingBaseSnapshot,
    #[error("coin mismatch: recorder={expected}, message={actual}")]
    CoinMismatch { expected: String, actual: String },
    #[error("duplicate order ID {0} in snapshot")]
    DuplicateOrder(u64),
    #[error("new diff for order {0} has no joined full order")]
    MissingNewOrder(u64),
    #[error("order {oid} state mismatch: {reason}")]
    StateMismatch { oid: u64, reason: String },
}

/// Minimal strict L4 recorder used by capture pipelines and the seamless
/// snapshot smoke test. Mutations are atomic per update batch.
#[derive(Debug, Clone)]
pub struct L4BookRecorder {
    coin: String,
    height: Option<u64>,
    exchange_time_ms: Option<u64>,
    orders: BTreeMap<u64, L4Order>,
}

impl L4BookRecorder {
    #[must_use]
    pub fn new(coin: impl Into<String>) -> Self {
        Self {
            coin: coin.into(),
            height: None,
            exchange_time_ms: None,
            orders: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn height(&self) -> Option<u64> {
        self.height
    }

    #[must_use]
    pub fn exchange_time_ms(&self) -> Option<u64> {
        self.exchange_time_ms
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    #[must_use]
    pub fn has_same_orders(&self, other: &Self) -> bool {
        self.coin == other.coin && self.orders == other.orders
    }

    /// Compares the reconstructable resting-book identity and fields that both
    /// Dwellir snapshot transports document. Provider-specific extras and an
    /// unavailable gRPC owner are intentionally excluded.
    #[must_use]
    pub fn has_same_resting_book(&self, other: &Self) -> bool {
        self.coin == other.coin
            && self.orders.len() == other.orders.len()
            && self.orders.iter().all(|(oid, left)| {
                other.orders.get(oid).is_some_and(|right| {
                    left.coin == right.coin
                        && left.side == right.side
                        && left.price == right.price
                        && left.size == right.size
                        && left.timestamp_ms == right.timestamp_ms
                        && left.cloid == right.cloid
                        && left.original_size == right.original_size
                })
            })
    }

    /// Returns the same deterministic order as authoritative snapshots.
    #[must_use]
    pub fn orders(&self) -> Vec<&L4Order> {
        let mut orders = self.orders.values().collect::<Vec<_>>();
        orders.sort_by(|left, right| compare_orders(left, right));
        orders
    }

    /// Replaces state for a newer or same-height snapshot. Older snapshots are
    /// identified and ignored without mutating current state.
    pub fn apply_snapshot(
        &mut self,
        snapshot: AuthoritativeL4Snapshot,
    ) -> Result<L4ApplyOutcome, L4ReconstructionError> {
        self.ensure_coin(&snapshot.coin)?;
        if self.height.is_some_and(|height| snapshot.height < height) {
            return Ok(L4ApplyOutcome::IgnoredStaleOrSameHeight);
        }
        let mut replacement = BTreeMap::new();
        for order in snapshot.orders {
            self.ensure_coin(&order.coin)?;
            let oid = order.oid;
            if replacement.insert(oid, order).is_some() {
                return Err(L4ReconstructionError::DuplicateOrder(oid));
            }
        }
        self.orders = replacement;
        self.height = Some(snapshot.height);
        self.exchange_time_ms = Some(snapshot.exchange_time_ms);
        Ok(L4ApplyOutcome::Applied)
    }

    /// Applies only a strictly newer complete update batch. The batch is
    /// validated against a clone before commit so partial mutations cannot
    /// leak on a gap/correlation error.
    pub fn apply_update(
        &mut self,
        update: &L4Updates,
    ) -> Result<L4ApplyOutcome, L4ReconstructionError> {
        let current_height = self
            .height
            .ok_or(L4ReconstructionError::MissingBaseSnapshot)?;
        if update.height <= current_height {
            return Ok(L4ApplyOutcome::IgnoredStaleOrSameHeight);
        }

        let mut next = self.orders.clone();
        for diff in &update.book_diffs {
            self.ensure_coin(&diff.coin)?;
            match &diff.raw_book_diff {
                super::types::RawBookDiff::New { sz } => {
                    let mut order = diff
                        .order
                        .clone()
                        .ok_or(L4ReconstructionError::MissingNewOrder(diff.oid))?;
                    self.ensure_coin(&order.coin)?;
                    if order.oid != diff.oid || order.price != diff.px {
                        return Err(L4ReconstructionError::StateMismatch {
                            oid: diff.oid,
                            reason: "joined order does not match diff OID/price".into(),
                        });
                    }
                    order.size = *sz;
                    if next.insert(diff.oid, order).is_some() {
                        return Err(L4ReconstructionError::StateMismatch {
                            oid: diff.oid,
                            reason: "new diff references an existing order".into(),
                        });
                    }
                }
                super::types::RawBookDiff::Update { orig_sz, new_sz } => {
                    let order = existing_order_mut(&mut next, diff.oid)?;
                    if order.size != *orig_sz {
                        return Err(L4ReconstructionError::StateMismatch {
                            oid: diff.oid,
                            reason: format!(
                                "expected original size {orig_sz}, recorded {}",
                                order.size
                            ),
                        });
                    }
                    order.size = *new_sz;
                }
                super::types::RawBookDiff::Modified { sz } => {
                    let order = existing_order_mut(&mut next, diff.oid)?;
                    order.price = diff.px;
                    order.size = *sz;
                }
                super::types::RawBookDiff::Remove => {
                    if next.remove(&diff.oid).is_none() {
                        return Err(L4ReconstructionError::StateMismatch {
                            oid: diff.oid,
                            reason: "remove diff references a missing order".into(),
                        });
                    }
                }
            }
        }
        self.orders = next;
        self.height = Some(update.height);
        self.exchange_time_ms = Some(update.time);
        Ok(L4ApplyOutcome::Applied)
    }

    fn ensure_coin(&self, actual: &str) -> Result<(), L4ReconstructionError> {
        if actual == self.coin {
            Ok(())
        } else {
            Err(L4ReconstructionError::CoinMismatch {
                expected: self.coin.clone(),
                actual: actual.to_owned(),
            })
        }
    }
}

fn existing_order_mut(
    orders: &mut BTreeMap<u64, L4Order>,
    oid: u64,
) -> Result<&mut L4Order, L4ReconstructionError> {
    orders
        .get_mut(&oid)
        .ok_or_else(|| L4ReconstructionError::StateMismatch {
            oid,
            reason: "diff references a missing order".into(),
        })
}

/// Provenance for one explicit snapshot request.
#[derive(Debug, Clone)]
pub struct SnapshotObservation {
    /// SDK-generated correlation ID for this caller request.
    pub request_id: String,
    /// Provider-echoed request ID, if the protocol supplies one.
    pub provider_request_id: Option<String>,
    pub connection_id: Option<String>,
    pub subscription_id: Option<String>,
    /// Local observation time, explicitly separate from exchange authority.
    pub receipt_time_ms: u64,
    /// Always one today: snapshot APIs perform no hidden retries.
    pub attempts: u32,
    /// Number of protocol operations (snapshot plus, for legacy WebSocket
    /// payloads, an exact-height block-time lookup). This is not a retry count.
    pub provider_operations: u32,
    pub snapshot: AuthoritativeL4Snapshot,
}

/// Snapshot and order-field support implemented by this client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L4Capabilities {
    pub authoritative_snapshot: bool,
    pub snapshot_exchange_timestamp: bool,
    pub order_timestamp: bool,
    pub order_cloid: bool,
    pub provider_sequence: bool,
    pub request_correlation: bool,
}

/// Runtime capability discovery result. A successful observation is retained
/// so startup validation can use it as the initial book instead of issuing a
/// second large request.
#[derive(Debug)]
pub struct L4CapabilityDiscovery {
    pub capabilities: L4Capabilities,
    pub observation: Option<SnapshotObservation>,
    pub failure: Option<String>,
}

impl L4Capabilities {
    /// Capabilities of strict isolated capture. Provider entitlement and
    /// exact-height timestamp lookup are validated by runtime discovery.
    pub const STRICT_AUTHORITATIVE: Self = Self {
        authoritative_snapshot: true,
        snapshot_exchange_timestamp: true,
        order_timestamp: true,
        order_cloid: true,
        provider_sequence: false,
        request_correlation: false,
    };
}

/// Typed strict-snapshot failure. No variant falls back to local state.
#[derive(Debug, thiserror::Error)]
pub enum L4SnapshotError {
    #[error("authoritative L4 snapshot is unsupported by this provider endpoint")]
    Unsupported,
    #[error("authoritative L4 snapshot request timed out")]
    Timeout,
    #[error("authoritative L4 snapshot request was cancelled")]
    Cancelled,
    #[error("provider snapshot has no exchange/node timestamp")]
    MissingExchangeTimestamp,
    #[error("provider returned an incomplete snapshot: {0}")]
    IncompleteSnapshot(String),
    #[error("snapshot correlation mismatch: {0}")]
    CorrelationMismatch(String),
    #[error("provider rejected snapshot request: {0}")]
    Provider(String),
    #[error("snapshot transport failed: {0}")]
    Transport(String),
    #[error("snapshot decoding failed: {0}")]
    Decode(String),
}

/// Cloneable, concurrency-bounded client for explicit snapshots.
#[derive(Clone, Debug)]
pub struct L4SnapshotClient {
    ws_endpoint: Url,
    grpc_endpoint: String,
    api_key: Option<String>,
    limiter: Arc<Semaphore>,
    request_timeout: Duration,
}

impl L4SnapshotClient {
    pub fn new(ws_endpoint: Url, grpc_endpoint: String, api_key: Option<String>) -> Self {
        Self {
            ws_endpoint,
            grpc_endpoint,
            api_key,
            limiter: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_SNAPSHOTS)),
            request_timeout: DEFAULT_SNAPSHOT_TIMEOUT,
        }
    }

    /// Sets the shared maximum number of in-flight requests (minimum one).
    #[must_use]
    pub fn with_max_concurrent_requests(mut self, maximum: usize) -> Self {
        self.limiter = Arc::new(Semaphore::new(maximum.max(1)));
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    #[must_use]
    pub const fn capabilities(&self) -> L4Capabilities {
        L4Capabilities::STRICT_AUTHORITATIVE
    }

    /// Actively proves endpoint entitlement, timestamp presence, decoding,
    /// and requested-coin correlation. This is provider discovery rather than
    /// an SDK-version assumption.
    pub async fn discover_capabilities(&self, coin: &str) -> L4CapabilityDiscovery {
        match self
            .fetch_l4_snapshot_observation(coin, CancellationToken::new())
            .await
        {
            Ok(observation) => L4CapabilityDiscovery {
                capabilities: L4Capabilities::STRICT_AUTHORITATIVE,
                observation: Some(observation),
                failure: None,
            },
            Err(error) => L4CapabilityDiscovery {
                capabilities: L4Capabilities {
                    authoritative_snapshot: false,
                    snapshot_exchange_timestamp: false,
                    order_timestamp: false,
                    order_cloid: false,
                    provider_sequence: false,
                    request_correlation: false,
                },
                observation: None,
                failure: Some(error.to_string()),
            },
        }
    }

    pub async fn fetch_l4_snapshot(
        &self,
        coin: &str,
    ) -> Result<AuthoritativeL4Snapshot, L4SnapshotError> {
        Ok(self
            .fetch_l4_snapshot_observation(coin, CancellationToken::new())
            .await?
            .snapshot)
    }

    /// Opens an isolated L4 subscription and obtains authority time either
    /// directly from that snapshot or from Dwellir `GetBlock` at the exact
    /// reported height. Dropping/cancelling closes all temporary transports.
    pub async fn fetch_l4_snapshot_observation(
        &self,
        coin: &str,
        cancellation: CancellationToken,
    ) -> Result<SnapshotObservation, L4SnapshotError> {
        let request_id = next_request_id();
        let operation = async {
            let _permit = self
                .limiter
                .acquire()
                .await
                .map_err(|_| L4SnapshotError::Transport("snapshot client closed".into()))?;
            let snapshot = fetch_first_l4_snapshot(self.ws_endpoint.clone(), coin)
                .await
                .map_err(map_ws_error)?;
            let (exchange_time_ms, exchange_time_source, provider_operations) =
                if let Some(time) = snapshot.time {
                    (time, SnapshotExchangeTimeSource::SnapshotPayload, 1)
                } else {
                    let raw = get_block_at_height(
                        &self.grpc_endpoint,
                        self.api_key.as_deref(),
                        snapshot.height,
                    )
                    .await
                    .map_err(map_rpc_error)?;
                    (
                        decode_exact_block_time(&raw, snapshot.height)?,
                        SnapshotExchangeTimeSource::ExactHeightBlockRpc,
                        2,
                    )
                };
            let decoded = decoded_from_ws(snapshot, coin, exchange_time_ms, exchange_time_source)?;
            Ok(build_observation(
                request_id,
                decoded,
                SnapshotAuthority::FreshSubscription,
                provider_operations,
            ))
        };

        tokio::select! {
            _ = cancellation.cancelled() => Err(L4SnapshotError::Cancelled),
            result = timeout(self.request_timeout, operation) => {
                result.map_err(|_| L4SnapshotError::Timeout)?
            }
        }
    }

    /// Explicit alias for the default fresh-subscription capture path.
    pub async fn fetch_l4_snapshot_via_subscription(
        &self,
        coin: &str,
        cancellation: CancellationToken,
    ) -> Result<SnapshotObservation, L4SnapshotError> {
        self.fetch_l4_snapshot_observation(coin, cancellation).await
    }

    /// Uses Dwellir's all-market `GetOrderBookSnapshot` RPC directly. This can
    /// be useful on premium endpoints configured for very large responses.
    pub async fn fetch_l4_snapshot_via_provider_rpc(
        &self,
        coin: &str,
        cancellation: CancellationToken,
    ) -> Result<SnapshotObservation, L4SnapshotError> {
        let request_id = next_request_id();
        let operation = async {
            let _permit = self
                .limiter
                .acquire()
                .await
                .map_err(|_| L4SnapshotError::Transport("snapshot client closed".into()))?;
            let raw = get_latest_orderbook_snapshot(&self.grpc_endpoint, self.api_key.as_deref())
                .await
                .map_err(map_rpc_error)?;
            let decoded = decode_provider_snapshot(&raw, coin)?;
            Ok(build_observation(
                request_id,
                decoded,
                SnapshotAuthority::ProviderRpc,
                1,
            ))
        };

        tokio::select! {
            _ = cancellation.cancelled() => Err(L4SnapshotError::Cancelled),
            result = timeout(self.request_timeout, operation) => {
                result.map_err(|_| L4SnapshotError::Timeout)?
            }
        }
    }
}

struct DecodedSnapshot {
    coin: String,
    height: u64,
    exchange_time_ms: u64,
    exchange_time_source: SnapshotExchangeTimeSource,
    orders: Vec<L4Order>,
    metadata: L4MessageMetadata,
}

fn build_observation(
    request_id: String,
    decoded: DecodedSnapshot,
    authority: SnapshotAuthority,
    provider_operations: u32,
) -> SnapshotObservation {
    let provider_request_id = decoded.metadata.request_id.clone();
    let connection_id = decoded.metadata.connection_id.clone();
    let subscription_id = decoded.metadata.subscription_id.clone();
    SnapshotObservation {
        request_id,
        provider_request_id,
        connection_id,
        subscription_id,
        receipt_time_ms: unix_time_ms(),
        attempts: 1,
        provider_operations,
        snapshot: AuthoritativeL4Snapshot {
            coin: decoded.coin,
            height: decoded.height,
            exchange_time_ms: decoded.exchange_time_ms,
            exchange_time_source: decoded.exchange_time_source,
            orders: decoded.orders,
            authority,
            metadata: decoded.metadata,
        },
    }
}

fn decoded_from_ws(
    snapshot: L4Snapshot,
    requested_coin: &str,
    exchange_time_ms: u64,
    exchange_time_source: SnapshotExchangeTimeSource,
) -> Result<DecodedSnapshot, L4SnapshotError> {
    if snapshot.coin != requested_coin {
        return Err(L4SnapshotError::CorrelationMismatch(format!(
            "requested {requested_coin}, received {}",
            snapshot.coin
        )));
    }
    let mut orders = snapshot.levels.into_iter().flatten().collect::<Vec<_>>();
    validate_and_sort_orders(&mut orders, requested_coin)?;
    Ok(DecodedSnapshot {
        coin: snapshot.coin,
        height: snapshot.height,
        exchange_time_ms,
        exchange_time_source,
        orders,
        metadata: snapshot.metadata,
    })
}

fn decode_provider_snapshot(
    raw: &[u8],
    requested_coin: &str,
) -> Result<DecodedSnapshot, L4SnapshotError> {
    let value: Value =
        serde_json::from_slice(raw).map_err(|err| L4SnapshotError::Decode(err.to_string()))?;
    let root = value.as_object().ok_or_else(|| {
        L4SnapshotError::IncompleteSnapshot("top-level payload is not an object".into())
    })?;

    let height = first_u64(root, &["height", "block", "blockHeight", "block_height"])
        .ok_or_else(|| L4SnapshotError::IncompleteSnapshot("missing block/book height".into()))?;
    let exchange_time_ms = first_u64(
        root,
        &["exchangeTimeMs", "exchange_time_ms", "timestamp", "time"],
    )
    .ok_or(L4SnapshotError::MissingExchangeTimestamp)?;
    let levels = find_market_levels(root, requested_coin)?;
    let mut orders = decode_sides(levels, requested_coin)?;
    validate_and_sort_orders(&mut orders, requested_coin)?;

    Ok(DecodedSnapshot {
        coin: requested_coin.to_owned(),
        height,
        exchange_time_ms,
        exchange_time_source: SnapshotExchangeTimeSource::ProviderSnapshotRpc,
        orders,
        metadata: metadata_from_object(root),
    })
}

fn decode_exact_block_time(raw: &[u8], requested_height: u64) -> Result<u64, L4SnapshotError> {
    let value: Value =
        serde_json::from_slice(raw).map_err(|err| L4SnapshotError::Decode(err.to_string()))?;
    let root = value.as_object().ok_or_else(|| {
        L4SnapshotError::IncompleteSnapshot("GetBlock payload is not an object".into())
    })?;
    let block = root
        .get("abci_block")
        .and_then(Value::as_object)
        .ok_or_else(|| L4SnapshotError::IncompleteSnapshot("GetBlock has no abci_block".into()))?;
    let height = first_u64(block, &["round", "height", "block_height"]).ok_or_else(|| {
        L4SnapshotError::IncompleteSnapshot("GetBlock has no block height".into())
    })?;
    if height != requested_height {
        return Err(L4SnapshotError::CorrelationMismatch(format!(
            "requested block {requested_height}, provider returned {height}"
        )));
    }
    let time = block
        .get("time")
        .ok_or(L4SnapshotError::MissingExchangeTimestamp)?;
    if let Some(ms) = time
        .as_u64()
        .or_else(|| time.as_str().and_then(|text| text.parse().ok()))
    {
        return Ok(ms);
    }
    let text = time
        .as_str()
        .ok_or(L4SnapshotError::MissingExchangeTimestamp)?;
    let parsed = DateTime::parse_from_rfc3339(text)
        .map(|time| time.to_utc())
        .or_else(|_| {
            NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f").map(|time| time.and_utc())
        })
        .map_err(|err| L4SnapshotError::Decode(format!("invalid GetBlock time: {err}")))?;
    u64::try_from(parsed.timestamp_millis())
        .map_err(|_| L4SnapshotError::Decode("GetBlock time is before the Unix epoch".into()))
}

fn find_market_levels<'a>(
    root: &'a Map<String, Value>,
    requested_coin: &str,
) -> Result<&'a Value, L4SnapshotError> {
    if root.get("coin").and_then(Value::as_str) == Some(requested_coin) {
        return root.get("levels").ok_or_else(|| {
            L4SnapshotError::IncompleteSnapshot("coin snapshot has no levels".into())
        });
    }

    for key in ["data", "levels", "markets"] {
        let Some(entries) = root.get(key).and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            if let Some(tuple) = entry.as_array() {
                if tuple.first().and_then(Value::as_str) == Some(requested_coin) {
                    return tuple.get(1).ok_or_else(|| {
                        L4SnapshotError::IncompleteSnapshot(format!(
                            "market tuple for {requested_coin} has no sides"
                        ))
                    });
                }
            }
            if let Some(market) = entry.as_object() {
                if market.get("coin").and_then(Value::as_str) == Some(requested_coin) {
                    if let Some(levels) = market.get("levels") {
                        return Ok(levels);
                    }
                    // Return the object itself; `decode_sides` understands
                    // `{coin,bids,asks}`.
                    return Ok(entry);
                }
            }
        }
    }

    Err(L4SnapshotError::CorrelationMismatch(format!(
        "provider snapshot does not contain requested coin {requested_coin}"
    )))
}

fn decode_sides(value: &Value, requested_coin: &str) -> Result<Vec<L4Order>, L4SnapshotError> {
    let (bids, asks) = if let Some(sides) = value.as_array() {
        if sides.len() != 2 {
            return Err(L4SnapshotError::IncompleteSnapshot(format!(
                "{requested_coin} has {} sides, expected 2",
                sides.len()
            )));
        }
        (&sides[0], &sides[1])
    } else if let Some(market) = value.as_object() {
        let bids = market.get("bids").ok_or_else(|| {
            L4SnapshotError::IncompleteSnapshot(format!("{requested_coin} has no bids array"))
        })?;
        let asks = market.get("asks").ok_or_else(|| {
            L4SnapshotError::IncompleteSnapshot(format!("{requested_coin} has no asks array"))
        })?;
        (bids, asks)
    } else {
        return Err(L4SnapshotError::IncompleteSnapshot(format!(
            "{requested_coin} sides are neither an array nor an object"
        )));
    };

    let mut orders = Vec::new();
    decode_side(bids, Side::Bid, requested_coin, &mut orders)?;
    decode_side(asks, Side::Ask, requested_coin, &mut orders)?;
    Ok(orders)
}

fn decode_side(
    value: &Value,
    expected_side: Side,
    coin: &str,
    output: &mut Vec<L4Order>,
) -> Result<(), L4SnapshotError> {
    let entries = value.as_array().ok_or_else(|| {
        L4SnapshotError::IncompleteSnapshot(format!("{coin} {expected_side} side is not an array"))
    })?;
    output.reserve(entries.len());
    for entry in entries {
        let order: L4Order = serde_json::from_value(entry.clone())
            .map_err(|err| L4SnapshotError::Decode(format!("{coin} order: {err}")))?;
        if order.side != expected_side {
            return Err(L4SnapshotError::IncompleteSnapshot(format!(
                "order {} is {} in the {expected_side} array",
                order.oid, order.side
            )));
        }
        output.push(order);
    }
    Ok(())
}

fn validate_and_sort_orders(
    orders: &mut [L4Order],
    requested_coin: &str,
) -> Result<(), L4SnapshotError> {
    for order in orders.iter() {
        if order.coin != requested_coin {
            return Err(L4SnapshotError::CorrelationMismatch(format!(
                "requested {requested_coin}, order {} belongs to {}",
                order.oid, order.coin
            )));
        }
    }
    orders.sort_by(compare_orders);
    Ok(())
}

fn compare_orders(left: &L4Order, right: &L4Order) -> Ordering {
    left.side
        .cmp(&right.side)
        .then_with(|| match left.side {
            Side::Bid => right.price.cmp(&left.price),
            Side::Ask => left.price.cmp(&right.price),
        })
        .then_with(|| left.timestamp_ms.cmp(&right.timestamp_ms))
        .then_with(|| left.oid.cmp(&right.oid))
}

fn metadata_from_object(root: &Map<String, Value>) -> L4MessageMetadata {
    L4MessageMetadata {
        connection_id: first_string(
            root,
            &["connectionId", "connection_id", "sessionId", "session_id"],
        ),
        subscription_id: first_string(root, &["subscriptionId", "subscription_id"]),
        provider_sequence: first_u64(
            root,
            &["providerSequence", "provider_sequence", "sequence", "seq"],
        ),
        request_id: first_string(root, &["requestId", "request_id"]),
        protocol_version: first_string(
            root,
            &[
                "protocolVersion",
                "protocol_version",
                "schemaVersion",
                "schema_version",
            ],
        ),
        checksum: first_string(root, &["checksum", "bookDigest", "book_digest"]),
        // Avoid cloning the all-market `data`/`levels` payload into metadata.
        // WebSocket message-level unknown fields are retained by serde.
        extra: Map::new(),
    }
}

fn first_u64(root: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        let value = root.get(*key)?;
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    })
}

fn first_string(root: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| root.get(*key).and_then(Value::as_str).map(str::to_owned))
}

fn map_rpc_error(error: SnapshotRpcError) -> L4SnapshotError {
    match error {
        SnapshotRpcError::Transport(err) => L4SnapshotError::Transport(err.to_string()),
        SnapshotRpcError::InvalidMetadata(err) => L4SnapshotError::Transport(err),
        SnapshotRpcError::Provider(status) => match status.code() {
            Code::Unimplemented | Code::NotFound => L4SnapshotError::Unsupported,
            Code::DeadlineExceeded => L4SnapshotError::Timeout,
            _ => L4SnapshotError::Provider(status.to_string()),
        },
    }
}

fn map_ws_error(error: OneShotSnapshotError) -> L4SnapshotError {
    match error {
        OneShotSnapshotError::Transport(err) => L4SnapshotError::Transport(err),
        OneShotSnapshotError::Provider(err) => L4SnapshotError::Provider(err),
        OneShotSnapshotError::Closed => {
            L4SnapshotError::Transport("provider closed before a complete snapshot".into())
        }
    }
}

fn next_request_id() -> String {
    format!(
        "hypersdk-l4-{}",
        REQUEST_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed)
    )
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::super::types::{L4BookDiff, RawBookDiff};
    use super::*;
    use alloy::primitives::Address;
    use rust_decimal::Decimal;

    const USER: &str = "0xf9109ada2f73c62e9889b45453065f0d99260a2d";

    fn order(
        oid: u64,
        side: &str,
        price: &str,
        timestamp: Option<u64>,
        cloid: Option<&str>,
    ) -> Value {
        serde_json::json!({
            "user": USER,
            "coin": "BTC",
            "side": side,
            "limitPx": price,
            "sz": "0.5",
            "oid": oid,
            "timestamp": timestamp,
            "triggerCondition": "N/A",
            "isTrigger": false,
            "triggerPx": "0",
            "isPositionTpsl": false,
            "reduceOnly": false,
            "orderType": "Limit",
            "tif": "Gtc",
            "cloid": cloid,
            "origSz": "0.75",
            "providerFutureField": 7
        })
    }

    #[test]
    fn decodes_authoritative_tuple_snapshot_and_sorts_deterministically() {
        let raw = serde_json::to_vec(&serde_json::json!({
            "block": "1057074308",
            "timestamp": 1782950413902_u64,
            "requestId": "provider-1",
            "schemaVersion": "2",
            "data": [["BTC", [
                [order(2, "B", "100", Some(2), None), order(1, "B", "101", Some(1), Some("abc"))],
                [order(3, "A", "102", Some(3), None)]
            ]]]
        }))
        .unwrap();

        let decoded = decode_provider_snapshot(&raw, "BTC").unwrap();
        assert_eq!(decoded.height, 1_057_074_308);
        assert_eq!(decoded.exchange_time_ms, 1_782_950_413_902);
        assert_eq!(
            decoded
                .orders
                .iter()
                .map(|order| order.oid)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(decoded.orders[0].timestamp_ms, Some(1));
        assert_eq!(decoded.orders[0].cloid.as_deref(), Some("abc"));
        assert_eq!(decoded.orders[0].original_size, Some(Decimal::new(75, 2)));
        assert_eq!(decoded.orders[0].extra["providerFutureField"], 7);
        assert_eq!(decoded.metadata.request_id.as_deref(), Some("provider-1"));
    }

    #[test]
    fn missing_exchange_timestamp_is_distinct() {
        let raw = serde_json::to_vec(&serde_json::json!({
            "height": 7,
            "data": [["BTC", [[], []]]]
        }))
        .unwrap();
        assert!(matches!(
            decode_provider_snapshot(&raw, "BTC"),
            Err(L4SnapshotError::MissingExchangeTimestamp)
        ));
    }

    #[test]
    fn malformed_incomplete_and_wrong_coin_snapshots_are_distinct() {
        assert!(matches!(
            decode_provider_snapshot(b"not-json", "BTC"),
            Err(L4SnapshotError::Decode(_))
        ));
        let incomplete = serde_json::to_vec(&serde_json::json!({
            "time": 1,
            "data": [["BTC", [[], []]]]
        }))
        .unwrap();
        assert!(matches!(
            decode_provider_snapshot(&incomplete, "BTC"),
            Err(L4SnapshotError::IncompleteSnapshot(_))
        ));
        let wrong_coin = serde_json::to_vec(&serde_json::json!({
            "height": 1,
            "time": 2,
            "data": [["ETH", [[], []]]]
        }))
        .unwrap();
        assert!(matches!(
            decode_provider_snapshot(&wrong_coin, "BTC"),
            Err(L4SnapshotError::CorrelationMismatch(_))
        ));
    }

    #[test]
    fn exact_height_block_time_is_provider_correlated() {
        let raw = br#"{
            "abci_block": {
                "time": "2026-07-20T12:34:56.789123456",
                "round": 1057074308
            }
        }"#;
        assert_eq!(
            decode_exact_block_time(raw, 1_057_074_308).unwrap(),
            1_784_550_896_789
        );
        assert!(matches!(
            decode_exact_block_time(raw, 1_057_074_309),
            Err(L4SnapshotError::CorrelationMismatch(_))
        ));
    }

    #[tokio::test]
    async fn pre_cancelled_snapshot_does_not_open_or_perturb_a_stream() {
        let client = L4SnapshotClient::new(
            Url::parse("ws://127.0.0.1:9/ws").unwrap(),
            "http://127.0.0.1:9".into(),
            None,
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            client
                .fetch_l4_snapshot_observation("BTC", cancellation)
                .await,
            Err(L4SnapshotError::Cancelled)
        ));
    }

    #[test]
    fn ordering_semantics_are_strictly_greater_for_updates() {
        let snapshot = AuthoritativeL4Snapshot {
            coin: "BTC".into(),
            height: 10,
            exchange_time_ms: 20,
            exchange_time_source: SnapshotExchangeTimeSource::ProviderSnapshotRpc,
            orders: vec![],
            authority: SnapshotAuthority::ProviderRpc,
            metadata: L4MessageMetadata::default(),
        };
        let update = |height| L4Updates {
            time: 0,
            height,
            order_statuses: vec![],
            book_diffs: vec![],
            metadata: L4MessageMetadata::default(),
        };
        assert!(!snapshot.accepts_update(&update(9)));
        assert!(!snapshot.accepts_update(&update(10)));
        assert!(snapshot.accepts_update(&update(11)));
        assert!(snapshot.is_stale_at(11));
        assert!(!snapshot.is_stale_at(10));
    }

    #[test]
    fn seamless_snapshot_interleave_rebuilds_identical_book() {
        let snapshot_at_10 = decode_provider_snapshot(
            &serde_json::to_vec(&serde_json::json!({
                "height": 10,
                "time": 1000,
                "data": [["BTC", [[
                    order(1, "B", "100", Some(1), Some("first"))
                ], [
                    order(2, "A", "102", Some(2), None)
                ]]]]
            }))
            .unwrap(),
            "BTC",
        )
        .unwrap();
        let snapshot_at_11 = decode_provider_snapshot(
            &serde_json::to_vec(&serde_json::json!({
                "height": 11,
                "time": 1100,
                "data": [["BTC", [[
                    order(3, "B", "99", Some(3), Some("new-order"))
                ], [
                    order(2, "A", "102", Some(2), None)
                ]]]]
            }))
            .unwrap(),
            "BTC",
        )
        .unwrap();

        let update_11: L4Updates = serde_json::from_value(serde_json::json!({
            "time": 1100,
            "height": 11,
            "order_statuses": [{
                "time": "2026-01-01T00:00:01Z",
                "user": USER,
                "status": "open",
                "order": order(3, "B", "99", Some(3), Some("new-order"))
            }],
            "book_diffs": [{
                "user": USER,
                "oid": 1,
                "px": "100",
                "coin": "BTC",
                "raw_book_diff": "remove"
            }, {
                "user": USER,
                "oid": 3,
                "px": "99",
                "coin": "BTC",
                "raw_book_diff": {"new": {"sz": "0.5"}}
            }]
        }))
        .unwrap();
        assert_eq!(
            update_11.book_diffs[1]
                .order
                .as_ref()
                .and_then(|order| order.timestamp_ms),
            Some(3)
        );
        assert_eq!(
            update_11.book_diffs[1]
                .order
                .as_ref()
                .and_then(|order| order.cloid.as_deref()),
            Some("new-order")
        );

        let update_12: L4Updates = serde_json::from_value(serde_json::json!({
            "time": 1200,
            "height": 12,
            "order_statuses": [],
            "book_diffs": [{
                "user": USER,
                "oid": 3,
                "px": "99",
                "coin": "BTC",
                "raw_book_diff": {"update": {"origSz": "0.5", "newSz": "0.25"}}
            }]
        }))
        .unwrap();

        let authoritative = |decoded: DecodedSnapshot| AuthoritativeL4Snapshot {
            coin: decoded.coin,
            height: decoded.height,
            exchange_time_ms: decoded.exchange_time_ms,
            exchange_time_source: decoded.exchange_time_source,
            orders: decoded.orders,
            authority: SnapshotAuthority::ProviderRpc,
            metadata: decoded.metadata,
        };
        let mut continuous = L4BookRecorder::new("BTC");
        continuous
            .apply_snapshot(authoritative(snapshot_at_10))
            .unwrap();
        continuous.apply_update(&update_11).unwrap();
        continuous.apply_update(&update_12).unwrap();

        let mut resnapshotted = L4BookRecorder::new("BTC");
        resnapshotted
            .apply_snapshot(authoritative(snapshot_at_11))
            .unwrap();
        assert_eq!(
            resnapshotted.apply_update(&update_11).unwrap(),
            L4ApplyOutcome::IgnoredStaleOrSameHeight
        );
        resnapshotted.apply_update(&update_12).unwrap();

        let fingerprint = |book: &L4BookRecorder| {
            book.orders()
                .into_iter()
                .map(|order| {
                    (
                        order.oid,
                        order.side,
                        order.price,
                        order.size,
                        order.timestamp_ms,
                        order.cloid.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(continuous.height(), Some(12));
        assert_eq!(fingerprint(&continuous), fingerprint(&resnapshotted));
    }

    #[test]
    fn ten_thousand_primary_updates_survive_a_midstream_resnapshot() {
        let base = decode_provider_snapshot(
            &serde_json::to_vec(&serde_json::json!({
                "height": 1,
                "time": 1,
                "data": [["BTC", [[order(1, "B", "100", Some(1), Some("stable"))], []]]]
            }))
            .unwrap(),
            "BTC",
        )
        .unwrap();
        let authoritative = |decoded: DecodedSnapshot| AuthoritativeL4Snapshot {
            coin: decoded.coin,
            height: decoded.height,
            exchange_time_ms: decoded.exchange_time_ms,
            exchange_time_source: decoded.exchange_time_source,
            orders: decoded.orders,
            authority: SnapshotAuthority::ProviderRpc,
            metadata: decoded.metadata,
        };
        let mut continuous = L4BookRecorder::new("BTC");
        continuous.apply_snapshot(authoritative(base)).unwrap();
        let user: Address = USER.parse().unwrap();
        let mut resnapshotted: Option<L4BookRecorder> = None;

        for index in 1..=10_000_u64 {
            let size = Decimal::from(index + 1);
            let update = L4Updates {
                time: index + 1,
                height: index + 1,
                order_statuses: vec![],
                book_diffs: vec![L4BookDiff {
                    user,
                    oid: 1,
                    px: Decimal::from(100),
                    coin: "BTC".into(),
                    raw_book_diff: RawBookDiff::Modified { sz: size },
                    order: None,
                    extra: Map::new(),
                }],
                metadata: L4MessageMetadata {
                    provider_sequence: Some(index),
                    ..Default::default()
                },
            };
            continuous.apply_update(&update).unwrap();
            if let Some(book) = &mut resnapshotted {
                book.apply_update(&update).unwrap();
            }
            if index == 5_000 {
                let midpoint = AuthoritativeL4Snapshot {
                    coin: "BTC".into(),
                    height: continuous.height().unwrap(),
                    exchange_time_ms: continuous.exchange_time_ms().unwrap(),
                    exchange_time_source: SnapshotExchangeTimeSource::ProviderSnapshotRpc,
                    orders: continuous.orders().into_iter().cloned().collect(),
                    authority: SnapshotAuthority::ProviderRpc,
                    metadata: L4MessageMetadata::default(),
                };
                let mut book = L4BookRecorder::new("BTC");
                book.apply_snapshot(midpoint).unwrap();
                resnapshotted = Some(book);
            }
        }

        let resnapshotted = resnapshotted.unwrap();
        assert_eq!(continuous.height(), Some(10_001));
        assert_eq!(resnapshotted.height(), Some(10_001));
        assert!(continuous.has_same_orders(&resnapshotted));
    }
}
