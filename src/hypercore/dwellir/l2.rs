//! Reconnecting gRPC client for Dwellir's `StreamL2BookDiff` endpoint.
//!
//! Dwellir's v3 `MarketStreaming` service
//! (`hyperliquid_l1_gateway.v3.MarketStreaming/StreamL2BookDiff`) streams
//! incremental aggregated (L2) book changes for up to 20 coins on one RPC.
//! The stream opens with one `snapshot: true` entry per coin (`seq: 1`,
//! `prev_seq: 0`) and then sends only changed levels per coalesce window,
//! chained per coin with `seq` / `prev_seq`.
//!
//! This module wraps that RPC with the same event/reconnect shape as the
//! fills connection in [`super::grpc`]:
//! - Exponential backoff on disconnect / transient failure.
//! - [`L2BookDiffEvent::Connected`] / [`L2BookDiffEvent::Disconnected`]
//!   lifecycle notifications.
//! - Every (re)connect re-opens the subscription, so the server sends fresh
//!   opening snapshots. There is no resume cursor; an
//!   [`L2BookRecorder`](super::L2BookRecorder) rebuilds automatically because
//!   a snapshot entry always resets that coin's book.
//! - Non-retryable RPC statuses (`INVALID_ARGUMENT`, `UNAUTHENTICATED`,
//!   `PERMISSION_DENIED`, `FAILED_PRECONDITION`, `UNIMPLEMENTED`) are surfaced
//!   once as a terminal [`L2BookDiffEvent::Error`], after which the stream
//!   ends. Retrying those in a loop cannot succeed without a config change.
//!
//! Only the protobuf messages and client needed for `StreamL2BookDiff` are
//! defined here — no `.proto` / `build.rs` dependency.

use std::{
    collections::HashSet,
    fmt,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use tokio::sync::{
    Notify,
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};
use tonic::{
    Code, IntoRequest, Response, Status, Streaming, client::Grpc, codec::ProstCodec,
    codegen::http::uri::PathAndQuery, metadata::MetadataValue, transport::Channel,
};

use super::{
    grpc::{backoff, build_channel},
    types::{L2BookDiffUpdate, L2DiffConversionError},
};

/// Maximum number of coins per `StreamL2BookDiff` subscription.
pub const L2_DIFF_MAX_COINS: usize = 20;
/// Maximum explicit `n_levels` (other than `0`, which requests full depth).
pub const L2_DIFF_MAX_LEVELS: u32 = 100;
/// Server default for `n_levels` when omitted.
pub const L2_DIFF_DEFAULT_LEVELS: u32 = 20;

// ---------------------------------------------------------------------------
// Prost messages (hand-written to avoid a protoc build dependency).
// ---------------------------------------------------------------------------

/// Raw protobuf messages of `hyperliquid_l1_gateway.v3` used by
/// `StreamL2BookDiff`.
///
/// These mirror the wire format exactly (strings for decimals, `int64`
/// envelope fields). Prefer the typed [`crate::hypercore::dwellir::L2BookDiffUpdate`]
/// and [`super::L2BookDiffRequest`] in application code.
pub mod wire {
    /// `L2BookDiffRequest`.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct L2BookDiffRequest {
        /// 1-20 distinct, case-sensitive coins.
        #[prost(string, repeated, tag = "1")]
        pub coins: Vec<String>,
        /// Omitted = 20; 1-100 bounded; explicit 0 = full depth.
        #[prost(uint32, optional, tag = "2")]
        pub n_levels: Option<u32>,
        /// 2, 3, 4 or 5.
        #[prost(uint32, optional, tag = "3")]
        pub n_sig_figs: Option<u32>,
        /// Only with `n_sig_figs == 5`; 2 or 5.
        #[prost(uint64, optional, tag = "4")]
        pub mantissa: Option<u64>,
    }

    /// `L2BookDiffUpdate`: one coalesce window.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct L2BookDiffUpdate {
        #[prost(int64, tag = "1")]
        pub time: i64,
        #[prost(int64, tag = "2")]
        pub block_number: i64,
        #[prost(message, repeated, tag = "3")]
        pub diffs: Vec<L2CoinDiff>,
    }

    /// `L2CoinDiff`: per-coin entry of an update.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct L2CoinDiff {
        #[prost(string, tag = "1")]
        pub coin: String,
        #[prost(uint64, tag = "2")]
        pub seq: u64,
        #[prost(uint64, tag = "3")]
        pub prev_seq: u64,
        #[prost(message, repeated, tag = "4")]
        pub bids: Vec<L2Level>,
        #[prost(message, repeated, tag = "5")]
        pub asks: Vec<L2Level>,
        #[prost(bool, tag = "6")]
        pub snapshot: bool,
    }

    /// `L2Level`: one aggregated price level.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct L2Level {
        #[prost(string, tag = "1")]
        pub px: String,
        #[prost(string, tag = "2")]
        pub sz: String,
        #[prost(uint32, tag = "3")]
        pub n: u32,
    }
}

// ---------------------------------------------------------------------------
// Public request type + validation.
// ---------------------------------------------------------------------------

/// Subscription parameters for `StreamL2BookDiff`.
///
/// Build with [`L2BookDiffRequest::new`] and the chained setters, then
/// [`validate`](Self::validate) (the connection constructors validate for
/// you). Validation mirrors Dwellir's documented request contract so bad
/// requests fail locally instead of as a server `INVALID_ARGUMENT`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct L2BookDiffRequest {
    /// Case-sensitive coins, 1-20 distinct values. Duplicates are removed by
    /// [`validate`](Self::validate) (first occurrence wins).
    pub coins: Vec<String>,
    /// Book depth per side. `None` = server default (20); `Some(0)` = full
    /// depth (only on endpoints that support it, otherwise the server
    /// answers `FAILED_PRECONDITION`); otherwise 1-100.
    pub n_levels: Option<u32>,
    /// Significant-figure price aggregation: 2, 3, 4 or 5.
    pub n_sig_figs: Option<u32>,
    /// Mantissa for `n_sig_figs == 5`: 2 or 5.
    pub mantissa: Option<u64>,
}

/// Client-side validation failure for an [`L2BookDiffRequest`].
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum L2BookDiffRequestError {
    #[error("at least one coin is required")]
    NoCoins,
    #[error("too many coins: {0} distinct (max {L2_DIFF_MAX_COINS})")]
    TooManyCoins(usize),
    #[error("coin names must be non-empty")]
    EmptyCoin,
    #[error("n_levels must be 0 (full depth) or 1..={L2_DIFF_MAX_LEVELS}, got {0}")]
    InvalidLevels(u32),
    #[error("n_sig_figs must be 2, 3, 4 or 5, got {0}")]
    InvalidSigFigs(u32),
    #[error("mantissa requires n_sig_figs = 5")]
    MantissaWithoutFiveSigFigs,
    #[error("mantissa must be 2 or 5, got {0}")]
    InvalidMantissa(u64),
}

impl L2BookDiffRequest {
    /// Request for `coins` with server-default depth and no aggregation.
    pub fn new<I, S>(coins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            coins: coins.into_iter().map(Into::into).collect(),
            n_levels: None,
            n_sig_figs: None,
            mantissa: None,
        }
    }

    /// Sets the per-side depth (`0` = full depth where supported).
    #[must_use]
    pub fn n_levels(mut self, n_levels: u32) -> Self {
        self.n_levels = Some(n_levels);
        self
    }

    /// Requests full book depth (`n_levels = 0`).
    #[must_use]
    pub fn full_depth(self) -> Self {
        self.n_levels(0)
    }

    /// Sets significant-figure aggregation (2-5).
    #[must_use]
    pub fn n_sig_figs(mut self, n_sig_figs: u32) -> Self {
        self.n_sig_figs = Some(n_sig_figs);
        self
    }

    /// Sets the mantissa (2 or 5; requires `n_sig_figs == 5`).
    #[must_use]
    pub fn mantissa(mut self, mantissa: u64) -> Self {
        self.mantissa = Some(mantissa);
        self
    }

    /// Validates the request and returns a normalized copy with duplicate
    /// coins removed (order of first occurrence preserved).
    pub fn validate(&self) -> Result<Self, L2BookDiffRequestError> {
        let mut seen = HashSet::new();
        let mut coins = Vec::with_capacity(self.coins.len());
        for coin in &self.coins {
            if coin.is_empty() {
                return Err(L2BookDiffRequestError::EmptyCoin);
            }
            if seen.insert(coin.as_str()) {
                coins.push(coin.clone());
            }
        }
        if coins.is_empty() {
            return Err(L2BookDiffRequestError::NoCoins);
        }
        if coins.len() > L2_DIFF_MAX_COINS {
            return Err(L2BookDiffRequestError::TooManyCoins(coins.len()));
        }
        if let Some(n) = self.n_levels.filter(|n| *n > L2_DIFF_MAX_LEVELS) {
            return Err(L2BookDiffRequestError::InvalidLevels(n));
        }
        if let Some(sig) = self.n_sig_figs.filter(|sig| !(2..=5).contains(sig)) {
            return Err(L2BookDiffRequestError::InvalidSigFigs(sig));
        }
        if let Some(mantissa) = self.mantissa {
            if self.n_sig_figs != Some(5) {
                return Err(L2BookDiffRequestError::MantissaWithoutFiveSigFigs);
            }
            if mantissa != 2 && mantissa != 5 {
                return Err(L2BookDiffRequestError::InvalidMantissa(mantissa));
            }
        }
        Ok(Self {
            coins,
            n_levels: self.n_levels,
            n_sig_figs: self.n_sig_figs,
            mantissa: self.mantissa,
        })
    }

    fn to_wire(&self) -> wire::L2BookDiffRequest {
        wire::L2BookDiffRequest {
            coins: self.coins.clone(),
            n_levels: self.n_levels,
            n_sig_figs: self.n_sig_figs,
            mantissa: self.mantissa,
        }
    }
}

// ---------------------------------------------------------------------------
// Thin gRPC client for `MarketStreaming/StreamL2BookDiff`.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MarketStreamingClient {
    inner: Grpc<Channel>,
}

impl MarketStreamingClient {
    fn new(channel: Channel) -> Self {
        // A full-depth 20-coin opening snapshot can be large.
        let inner = Grpc::new(channel).max_decoding_message_size(150 * 1024 * 1024);
        Self { inner }
    }

    async fn stream_l2_book_diff(
        &mut self,
        request: impl IntoRequest<wire::L2BookDiffRequest>,
    ) -> Result<Response<Streaming<wire::L2BookDiffUpdate>>, Status> {
        self.inner.ready().await.map_err(|e| {
            Status::new(
                tonic::Code::Unknown,
                format!("market streaming service not ready: {e}"),
            )
        })?;
        let codec: ProstCodec<wire::L2BookDiffRequest, wire::L2BookDiffUpdate> =
            ProstCodec::default();
        let path = PathAndQuery::from_static(
            "/hyperliquid_l1_gateway.v3.MarketStreaming/StreamL2BookDiff",
        );
        self.inner
            .server_streaming(request.into_request(), path, codec)
            .await
    }
}

// ---------------------------------------------------------------------------
// Public reconnecting connection.
// ---------------------------------------------------------------------------

/// Terminal failure of an [`L2BookDiffConnection`]. After this is emitted as
/// [`L2BookDiffEvent::Error`] the stream ends.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum L2BookDiffStreamError {
    /// The server rejected the subscription with a non-retryable status
    /// (`INVALID_ARGUMENT`, `UNAUTHENTICATED`, `PERMISSION_DENIED`,
    /// `FAILED_PRECONDITION`, or `UNIMPLEMENTED`).
    #[error("StreamL2BookDiff rejected ({code:?}): {message}")]
    Rejected { code: Code, message: String },
    /// The API key cannot be encoded as gRPC metadata.
    #[error("invalid API key metadata: {0}")]
    InvalidApiKey(String),
}

impl L2BookDiffStreamError {
    /// gRPC status code of a server rejection, if any.
    #[must_use]
    pub fn code(&self) -> Option<Code> {
        match self {
            Self::Rejected { code, .. } => Some(*code),
            Self::InvalidApiKey(_) => None,
        }
    }
}

/// Lifecycle + data events yielded by an [`L2BookDiffConnection`].
#[derive(Clone, Debug)]
pub enum L2BookDiffEvent {
    /// gRPC stream established (including after a reconnect). The next
    /// messages will be fresh `snapshot: true` entries for every coin.
    Connected,
    /// Stream dropped; a reconnect is already being attempted.
    Disconnected,
    /// One coalesce window of per-coin diffs.
    Message(L2BookDiffUpdate),
    /// Non-retryable failure. This is the last event; the stream then ends.
    Error(L2BookDiffStreamError),
}

/// Handle that can force an [`L2BookDiffConnection`] to reconnect, detached
/// from the event stream — see [`L2BookDiffConnection::split`].
#[derive(Clone, Debug)]
pub struct L2BookDiffHandle {
    reconnect: Arc<Notify>,
}

impl L2BookDiffHandle {
    /// Drops the current gRPC stream and re-subscribes. The connection emits
    /// [`L2BookDiffEvent::Disconnected`], then [`L2BookDiffEvent::Connected`]
    /// followed by fresh opening snapshots for every coin.
    ///
    /// Call this after an [`L2BookRecorder`](super::L2BookRecorder) /
    /// [`L2BookSet`](super::L2BookSet) reports a sequence gap.
    pub fn reconnect(&self) {
        self.reconnect.notify_one();
    }
}

/// Reconnecting gRPC subscription to Dwellir's `StreamL2BookDiff`.
///
/// Implements `futures::Stream<Item = L2BookDiffEvent>`.
///
/// # Recovery
///
/// The server closes the stream on its own frame loss (`ABORTED`) or when the
/// consumer is too slow (`DEADLINE_EXCEEDED`); the connection reconnects
/// automatically and the new stream opens with fresh snapshots, which
/// [`L2BookRecorder`](super::L2BookRecorder) applies without any manual reset.
/// If *your* recorder detects a sequence gap, call [`Self::reconnect`] (or
/// [`L2BookDiffHandle::reconnect`]) to force the same rebuild.
///
/// A frame that fails typed conversion (e.g. an unparsable decimal) is logged
/// and also triggers a reconnect, because skipping it would silently break
/// the per-coin sequence chain.
pub struct L2BookDiffConnection {
    rx: UnboundedReceiver<L2BookDiffEvent>,
    handle: L2BookDiffHandle,
    request: L2BookDiffRequest,
}

/// Event stream detached from the connection handle.
#[derive(Debug)]
pub struct L2BookDiffConnectionStream {
    rx: UnboundedReceiver<L2BookDiffEvent>,
}

impl fmt::Debug for L2BookDiffConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("L2BookDiffConnection")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

impl L2BookDiffConnection {
    /// Validates `request` and starts a connection to the given gRPC endpoint
    /// (e.g. `https://<node-host>:443`) with optional `x-api-key` metadata.
    ///
    /// Must be called from within a Tokio runtime.
    pub fn new(
        endpoint: String,
        api_key: Option<String>,
        request: L2BookDiffRequest,
    ) -> Result<Self, L2BookDiffRequestError> {
        let request = request.validate()?;
        let (tx, rx) = unbounded_channel();
        let reconnect = Arc::new(Notify::new());
        tokio::spawn(run(
            endpoint,
            api_key,
            request.clone(),
            tx,
            reconnect.clone(),
        ));
        Ok(Self {
            rx,
            handle: L2BookDiffHandle { reconnect },
            request,
        })
    }

    /// The validated (deduplicated) request this connection subscribes with.
    #[must_use]
    pub fn request(&self) -> &L2BookDiffRequest {
        &self.request
    }

    /// Forces a reconnect; see [`L2BookDiffHandle::reconnect`].
    pub fn reconnect(&self) {
        self.handle.reconnect();
    }

    /// Returns a cloneable handle that can force reconnects.
    #[must_use]
    pub fn handle(&self) -> L2BookDiffHandle {
        self.handle.clone()
    }

    /// Splits into a reconnect handle and the event stream.
    pub fn split(self) -> (L2BookDiffHandle, L2BookDiffConnectionStream) {
        (self.handle, L2BookDiffConnectionStream { rx: self.rx })
    }

    /// Splits off the event stream, dropping the reconnect handle.
    pub fn into_stream(self) -> L2BookDiffConnectionStream {
        L2BookDiffConnectionStream { rx: self.rx }
    }
}

impl futures::Stream for L2BookDiffConnection {
    type Item = L2BookDiffEvent;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_recv(cx)
    }
}

impl futures::Stream for L2BookDiffConnectionStream {
    type Item = L2BookDiffEvent;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_recv(cx)
    }
}

// ---------------------------------------------------------------------------
// Reconnect loop.
// ---------------------------------------------------------------------------

/// Statuses that retrying cannot fix without a configuration change.
fn is_fatal(code: Code) -> bool {
    matches!(
        code,
        Code::InvalidArgument
            | Code::Unauthenticated
            | Code::PermissionDenied
            | Code::FailedPrecondition
            | Code::Unimplemented
    )
}

/// Why the inner receive loop ended.
enum StreamEnd {
    /// Transient: reconnect after backoff.
    Retry,
    /// Caller asked for a reconnect: reconnect immediately.
    Requested,
    /// Non-retryable: emit the error and stop.
    Fatal(L2BookDiffStreamError),
    /// Consumer dropped the event stream.
    ConsumerGone,
}

async fn run(
    endpoint: String,
    api_key: Option<String>,
    request: L2BookDiffRequest,
    tx: UnboundedSender<L2BookDiffEvent>,
    reconnect: Arc<Notify>,
) {
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const INITIAL_BACKOFF_MS: u64 = 500;
    const MAX_BACKOFF_MS: u64 = 5_000;

    let api_key = match api_key.as_deref().map(MetadataValue::try_from).transpose() {
        Ok(value) => value,
        Err(err) => {
            log::error!("dwellir l2diff: invalid API key header value: {err:?}");
            let _ = tx.send(L2BookDiffEvent::Error(
                L2BookDiffStreamError::InvalidApiKey(err.to_string()),
            ));
            return;
        }
    };
    let wire_request = request.to_wire();
    let mut attempts: u32 = 0;

    loop {
        if tx.is_closed() {
            return;
        }
        let channel = match build_channel(&endpoint, CONNECT_TIMEOUT).await {
            Ok(ch) => ch,
            Err(err) => {
                log::error!("dwellir l2diff: channel to {endpoint} failed: {err:?}");
                backoff("l2diff", &mut attempts, INITIAL_BACKOFF_MS, MAX_BACKOFF_MS).await;
                continue;
            }
        };

        let mut client = MarketStreamingClient::new(channel);
        let mut req = wire_request.clone().into_request();
        if let Some(key) = &api_key {
            req.metadata_mut().insert("x-api-key", key.clone());
        }

        let mut stream = match client.stream_l2_book_diff(req).await {
            Ok(resp) => resp.into_inner(),
            Err(status) if is_fatal(status.code()) => {
                log::error!(
                    "dwellir l2diff: StreamL2BookDiff rejected ({:?}), not retrying: {}",
                    status.code(),
                    status.message()
                );
                let _ = tx.send(L2BookDiffEvent::Error(L2BookDiffStreamError::Rejected {
                    code: status.code(),
                    message: status.message().to_owned(),
                }));
                return;
            }
            Err(status) => {
                log::error!(
                    "dwellir l2diff: StreamL2BookDiff failed ({:?}): {}",
                    status.code(),
                    status.message()
                );
                backoff("l2diff", &mut attempts, INITIAL_BACKOFF_MS, MAX_BACKOFF_MS).await;
                continue;
            }
        };

        attempts = 0;
        if tx.send(L2BookDiffEvent::Connected).is_err() {
            return;
        }

        let end = loop {
            tokio::select! {
                message = stream.message() => match message {
                    Ok(Some(raw)) => match L2BookDiffUpdate::try_from(raw) {
                        Ok(update) => {
                            if tx.send(L2BookDiffEvent::Message(update)).is_err() {
                                break StreamEnd::ConsumerGone;
                            }
                        }
                        Err(err) => {
                            log_conversion_error(&err);
                            break StreamEnd::Retry;
                        }
                    },
                    Ok(None) => {
                        log::warn!("dwellir l2diff: server closed stream");
                        break StreamEnd::Retry;
                    }
                    Err(status) if is_fatal(status.code()) => {
                        log::error!(
                            "dwellir l2diff: stream closed with non-retryable status ({:?}): {}",
                            status.code(),
                            status.message()
                        );
                        break StreamEnd::Fatal(L2BookDiffStreamError::Rejected {
                            code: status.code(),
                            message: status.message().to_owned(),
                        });
                    }
                    Err(status) => {
                        log::warn!(
                            "dwellir l2diff: stream error ({:?}): {}",
                            status.code(),
                            status.message()
                        );
                        break StreamEnd::Retry;
                    }
                },
                _ = reconnect.notified() => {
                    log::info!("dwellir l2diff: reconnect requested");
                    break StreamEnd::Requested;
                }
                _ = tx.closed() => break StreamEnd::ConsumerGone,
            }
        };
        drop(stream);

        match end {
            StreamEnd::ConsumerGone => return,
            StreamEnd::Fatal(err) => {
                let _ = tx.send(L2BookDiffEvent::Disconnected);
                let _ = tx.send(L2BookDiffEvent::Error(err));
                return;
            }
            StreamEnd::Requested => {
                if tx.send(L2BookDiffEvent::Disconnected).is_err() {
                    return;
                }
            }
            StreamEnd::Retry => {
                if tx.send(L2BookDiffEvent::Disconnected).is_err() {
                    return;
                }
                backoff("l2diff", &mut attempts, INITIAL_BACKOFF_MS, MAX_BACKOFF_MS).await;
            }
        }
    }
}

fn log_conversion_error(err: &L2DiffConversionError) {
    log::warn!("dwellir l2diff: dropping unparsable frame and resubscribing: {err}");
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;

    #[test]
    fn validate_accepts_and_dedups() {
        let req = L2BookDiffRequest::new(["BTC", "ETH", "BTC"])
            .n_levels(50)
            .n_sig_figs(5)
            .mantissa(2);
        let valid = req.validate().unwrap();
        assert_eq!(valid.coins, vec!["BTC".to_string(), "ETH".to_string()]);
        assert_eq!(valid.n_levels, Some(50));
        assert!(
            L2BookDiffRequest::new(["BTC"])
                .full_depth()
                .validate()
                .is_ok()
        );
        assert!(
            L2BookDiffRequest::new(["BTC"])
                .n_levels(100)
                .validate()
                .is_ok()
        );
        assert!(
            L2BookDiffRequest::new(["BTC"])
                .n_sig_figs(2)
                .validate()
                .is_ok()
        );
        // Case-sensitive: these are distinct coins.
        assert_eq!(
            L2BookDiffRequest::new(["kPEPE", "KPEPE"])
                .validate()
                .unwrap()
                .coins
                .len(),
            2
        );
    }

    #[test]
    fn validate_rejects_bad_requests() {
        use L2BookDiffRequestError as E;
        let none: [&str; 0] = [];
        assert_eq!(L2BookDiffRequest::new(none).validate(), Err(E::NoCoins));
        assert_eq!(L2BookDiffRequest::new([""]).validate(), Err(E::EmptyCoin));
        let many = (0..21).map(|i| format!("C{i}"));
        assert_eq!(
            L2BookDiffRequest::new(many).validate(),
            Err(E::TooManyCoins(21))
        );
        // 21 entries but only 20 distinct is fine.
        let dup = (0..20).map(|i| format!("C{i}")).chain(["C0".to_string()]);
        assert!(L2BookDiffRequest::new(dup).validate().is_ok());
        assert_eq!(
            L2BookDiffRequest::new(["BTC"]).n_levels(101).validate(),
            Err(E::InvalidLevels(101))
        );
        assert_eq!(
            L2BookDiffRequest::new(["BTC"]).n_sig_figs(1).validate(),
            Err(E::InvalidSigFigs(1))
        );
        assert_eq!(
            L2BookDiffRequest::new(["BTC"]).n_sig_figs(6).validate(),
            Err(E::InvalidSigFigs(6))
        );
        assert_eq!(
            L2BookDiffRequest::new(["BTC"]).mantissa(2).validate(),
            Err(E::MantissaWithoutFiveSigFigs)
        );
        assert_eq!(
            L2BookDiffRequest::new(["BTC"])
                .n_sig_figs(4)
                .mantissa(2)
                .validate(),
            Err(E::MantissaWithoutFiveSigFigs)
        );
        assert_eq!(
            L2BookDiffRequest::new(["BTC"])
                .n_sig_figs(5)
                .mantissa(3)
                .validate(),
            Err(E::InvalidMantissa(3))
        );
    }

    #[test]
    fn request_encodes_explicit_zero_levels_and_omits_unset() {
        let req = L2BookDiffRequest::new(["BTC"]).full_depth().to_wire();
        // field 1 (len) "BTC", field 2 (varint) 0 — present because optional.
        assert_eq!(
            req.encode_to_vec(),
            vec![0x0a, 0x03, b'B', b'T', b'C', 0x10, 0x00]
        );
        let req = L2BookDiffRequest::new(["BTC"]).to_wire();
        assert_eq!(req.encode_to_vec(), vec![0x0a, 0x03, b'B', b'T', b'C']);
        let req = L2BookDiffRequest::new(["A"])
            .n_levels(5)
            .n_sig_figs(5)
            .mantissa(2)
            .to_wire();
        assert_eq!(
            req.encode_to_vec(),
            vec![0x0a, 0x01, b'A', 0x10, 0x05, 0x18, 0x05, 0x20, 0x02]
        );
    }

    #[test]
    fn update_decodes_hand_encoded_bytes() {
        // L2Level { px: "1", sz: "2", n: 3 }
        let level = [0x0a, 0x01, b'1', 0x12, 0x01, b'2', 0x18, 0x03];
        // L2CoinDiff { coin: "BTC", seq: 2, prev_seq: 1, bids: [level], asks: [level], snapshot: true }
        let mut coin_diff = vec![0x0a, 0x03, b'B', b'T', b'C', 0x10, 0x02, 0x18, 0x01];
        coin_diff.extend([0x22, level.len() as u8]);
        coin_diff.extend(level);
        coin_diff.extend([0x2a, level.len() as u8]);
        coin_diff.extend(level);
        coin_diff.extend([0x30, 0x01]);
        // L2BookDiffUpdate { time: 7, block_number: 9, diffs: [coin_diff] }
        let mut update = vec![0x08, 0x07, 0x10, 0x09, 0x1a, coin_diff.len() as u8];
        update.extend(&coin_diff);

        let decoded = wire::L2BookDiffUpdate::decode(update.as_slice()).unwrap();
        let expected_level = wire::L2Level {
            px: "1".into(),
            sz: "2".into(),
            n: 3,
        };
        let expected = wire::L2BookDiffUpdate {
            time: 7,
            block_number: 9,
            diffs: vec![wire::L2CoinDiff {
                coin: "BTC".into(),
                seq: 2,
                prev_seq: 1,
                bids: vec![expected_level.clone()],
                asks: vec![expected_level],
                snapshot: true,
            }],
        };
        assert_eq!(decoded, expected);
        // And the round trip reproduces the same bytes.
        assert_eq!(expected.encode_to_vec(), update);
    }

    #[tokio::test]
    async fn invalid_api_key_is_terminal_without_network() {
        use futures::StreamExt;

        let mut conn = L2BookDiffConnection::new(
            "https://127.0.0.1:1".into(),
            Some("bad\nkey".into()),
            L2BookDiffRequest::new(["BTC"]),
        )
        .unwrap();
        assert!(matches!(
            conn.next().await,
            Some(L2BookDiffEvent::Error(
                L2BookDiffStreamError::InvalidApiKey(_)
            ))
        ));
        assert!(conn.next().await.is_none());
    }

    #[test]
    fn connection_rejects_invalid_request_up_front() {
        let err = L2BookDiffConnection::new("https://x:443".into(), None, {
            L2BookDiffRequest::new(["BTC"]).n_levels(500)
        })
        .unwrap_err();
        assert_eq!(err, L2BookDiffRequestError::InvalidLevels(500));
    }

    #[test]
    fn fatal_codes() {
        for code in [
            Code::InvalidArgument,
            Code::Unauthenticated,
            Code::PermissionDenied,
            Code::FailedPrecondition,
            Code::Unimplemented,
        ] {
            assert!(is_fatal(code), "{code:?}");
        }
        for code in [
            Code::Aborted,
            Code::DeadlineExceeded,
            Code::Unavailable,
            Code::ResourceExhausted,
            Code::Unknown,
            Code::Internal,
        ] {
            assert!(!is_fatal(code), "{code:?}");
        }
    }
}
