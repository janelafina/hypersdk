//! Reconnecting gRPC client for Dwellir's `StreamFills` endpoint.
//!
//! Dwellir exposes fills as a gRPC server-streaming RPC
//! (`dwellir.hyperliquid.v2.HyperliquidL1Gateway/StreamFills`). Each message
//! on the stream is a `BlockFills { bytes data }` whose `data` is a JSON
//! payload matching Hyperliquid's `node_fills` shape.
//!
//! This module wraps that RPC with the same event/reconnect semantics as the
//! WebSocket side:
//! - Exponential backoff on disconnect.
//! - `Event::Connected` / `Event::Disconnected` lifecycle notifications.
//! - After the first successful message, subsequent reconnects resume from
//!   `last_block_number + 1` so long-running consumers don't lose fills
//!   across transient disconnects (subject to Dwellir's 24h retention).
//!
//! Only the gRPC messages and client needed for `StreamFills` are defined
//! here — no `.proto` / `build.rs` dependency.

use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    time::sleep,
};
use tonic::{
    IntoRequest, Response, Status, Streaming,
    client::Grpc,
    codec::ProstCodec,
    codegen::http::uri::PathAndQuery,
    metadata::MetadataValue,
    transport::{Channel, ClientTlsConfig, Endpoint},
};

use super::types::FillsBlock;

// ---------------------------------------------------------------------------
// Prost messages (hand-written to avoid a protoc build dependency).
// ---------------------------------------------------------------------------

/// `Position` request for `StreamFills`. Empty means "start from latest".
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Position {
    #[prost(oneof = "position::Position", tags = "1, 2")]
    pub position: Option<position::Position>,
}

pub mod position {
    /// `oneof` body of [`super::Position`].
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Position {
        /// ms since Unix epoch, inclusive.
        #[prost(int64, tag = "1")]
        TimestampMs(i64),
        /// block height, inclusive.
        #[prost(int64, tag = "2")]
        BlockHeight(i64),
    }
}

/// Raw gRPC message as sent by Dwellir — `data` is JSON.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RawBlockFills {
    #[prost(bytes = "vec", tag = "1")]
    pub data: Vec<u8>,
}

/// Timestamp request for `GetOrderBookSnapshot`. Zero requests the latest
/// provider snapshot.
#[derive(Clone, PartialEq, ::prost::Message)]
pub(crate) struct SnapshotTimestamp {
    #[prost(int64, tag = "1")]
    pub timestamp_ms: i64,
}

/// Raw gRPC snapshot response; `data` is provider JSON.
#[derive(Clone, PartialEq, ::prost::Message)]
pub(crate) struct RawOrderBookSnapshot {
    #[prost(bytes = "vec", tag = "1")]
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Thin gRPC client for `HyperliquidL1Gateway/StreamFills`.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct HyperliquidL1GatewayClient {
    inner: Grpc<Channel>,
}

impl HyperliquidL1GatewayClient {
    fn new(channel: Channel) -> Self {
        // 150 MiB matches Dwellir's Go example's default — full books can be large.
        let inner = Grpc::new(channel).max_decoding_message_size(150 * 1024 * 1024);
        Self { inner }
    }

    async fn stream_fills(
        &mut self,
        request: impl IntoRequest<Position>,
    ) -> Result<Response<Streaming<RawBlockFills>>, Status> {
        self.inner.ready().await.map_err(|e| {
            Status::new(
                tonic::Code::Unknown,
                format!("gateway service not ready: {e}"),
            )
        })?;
        let codec: ProstCodec<Position, RawBlockFills> = ProstCodec::default();
        let path = PathAndQuery::from_static(
            "/hyperliquid_l1_gateway.v2.HyperliquidL1Gateway/StreamFills",
        );
        self.inner
            .server_streaming(request.into_request(), path, codec)
            .await
    }
}

#[derive(Clone)]
struct SnapshotGatewayClient {
    inner: Grpc<Channel>,
}

impl SnapshotGatewayClient {
    fn new(channel: Channel) -> Self {
        // Dwellir recommends 500 MiB for the all-market snapshot. Keep a hard
        // ceiling so malformed/provider responses cannot grow without bound.
        let inner = Grpc::new(channel).max_decoding_message_size(512 * 1024 * 1024);
        Self { inner }
    }

    async fn get_orderbook_snapshot(
        &mut self,
        request: impl IntoRequest<SnapshotTimestamp>,
    ) -> Result<Response<RawOrderBookSnapshot>, Status> {
        self.inner.ready().await.map_err(|e| {
            Status::new(
                tonic::Code::Unknown,
                format!("gateway service not ready: {e}"),
            )
        })?;
        let codec: ProstCodec<SnapshotTimestamp, RawOrderBookSnapshot> = ProstCodec::default();
        let path = PathAndQuery::from_static(
            "/hyperliquid_l1_gateway.v2.HyperliquidL1Gateway/GetOrderBookSnapshot",
        );
        self.inner.unary(request.into_request(), path, codec).await
    }

    async fn get_block(
        &mut self,
        request: impl IntoRequest<Position>,
    ) -> Result<Response<RawOrderBookSnapshot>, Status> {
        self.inner.ready().await.map_err(|e| {
            Status::new(
                tonic::Code::Unknown,
                format!("gateway service not ready: {e}"),
            )
        })?;
        let codec: ProstCodec<Position, RawOrderBookSnapshot> = ProstCodec::default();
        let path =
            PathAndQuery::from_static("/hyperliquid_l1_gateway.v2.HyperliquidL1Gateway/GetBlock");
        self.inner.unary(request.into_request(), path, codec).await
    }
}

/// Failure from the transport/RPC layer of a one-shot snapshot request.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SnapshotRpcError {
    #[error("transport: {0}")]
    Transport(#[source] anyhow::Error),
    #[error("provider: {0}")]
    Provider(#[source] Status),
    #[error("invalid API key metadata: {0}")]
    InvalidMetadata(String),
}

/// Opens an isolated channel, requests the latest all-market provider
/// snapshot once, and closes the channel when this future completes or drops.
pub(crate) async fn get_latest_orderbook_snapshot(
    endpoint: &str,
    api_key: Option<&str>,
) -> Result<Vec<u8>, SnapshotRpcError> {
    let channel = build_channel(endpoint, Duration::from_secs(10))
        .await
        .map_err(SnapshotRpcError::Transport)?;
    let mut client = SnapshotGatewayClient::new(channel);
    let mut request = SnapshotTimestamp { timestamp_ms: 0 }.into_request();
    if let Some(key) = api_key {
        let value = MetadataValue::try_from(key)
            .map_err(|err| SnapshotRpcError::InvalidMetadata(err.to_string()))?;
        request.metadata_mut().insert("x-api-key", value);
    }
    let response = client
        .get_orderbook_snapshot(request)
        .await
        .map_err(SnapshotRpcError::Provider)?;
    Ok(response.into_inner().data)
}

/// Fetches node block data for one exact height on an isolated gRPC channel.
pub(crate) async fn get_block_at_height(
    endpoint: &str,
    api_key: Option<&str>,
    height: u64,
) -> Result<Vec<u8>, SnapshotRpcError> {
    let height = i64::try_from(height).map_err(|_| {
        SnapshotRpcError::InvalidMetadata("block height exceeds provider i64 range".into())
    })?;
    let channel = build_channel(endpoint, Duration::from_secs(10))
        .await
        .map_err(SnapshotRpcError::Transport)?;
    let mut client = SnapshotGatewayClient::new(channel);
    let mut request = Position {
        position: Some(position::Position::BlockHeight(height)),
    }
    .into_request();
    if let Some(key) = api_key {
        let value = MetadataValue::try_from(key)
            .map_err(|err| SnapshotRpcError::InvalidMetadata(err.to_string()))?;
        request.metadata_mut().insert("x-api-key", value);
    }
    let response = client
        .get_block(request)
        .await
        .map_err(SnapshotRpcError::Provider)?;
    Ok(response.into_inner().data)
}

// ---------------------------------------------------------------------------
// Public reconnecting connection.
// ---------------------------------------------------------------------------

/// Lifecycle + data events yielded by a [`FillsConnection`].
#[derive(Clone, Debug)]
pub enum Event {
    /// gRPC channel + stream established (including after a reconnect).
    Connected,
    /// Stream dropped; reconnect is already being attempted.
    Disconnected,
    /// A block of fills received from the node.
    Message(FillsBlock),
}

/// Starting point for the fills stream.
#[derive(Clone, Debug, Default)]
pub enum StartPosition {
    /// Subscribe from the latest fills.
    #[default]
    Latest,
    /// Resume from a specific Unix millisecond timestamp (inclusive).
    TimestampMs(i64),
    /// Resume from a specific block height (inclusive).
    BlockHeight(i64),
}

impl StartPosition {
    fn to_request(&self) -> Position {
        match *self {
            StartPosition::Latest => Position { position: None },
            StartPosition::TimestampMs(ts) => Position {
                position: Some(position::Position::TimestampMs(ts)),
            },
            StartPosition::BlockHeight(h) => Position {
                position: Some(position::Position::BlockHeight(h)),
            },
        }
    }
}

/// Reconnecting gRPC subscription to Dwellir's `StreamFills`.
pub struct FillsConnection {
    rx: UnboundedReceiver<Event>,
}

/// Event stream detached from the connection handle.
#[derive(Debug)]
pub struct FillsConnectionStream {
    rx: UnboundedReceiver<Event>,
}

impl FillsConnection {
    /// Starts a connection using the given gRPC endpoint (e.g.
    /// `https://hyperliquid.dwellir.com:443`) and optional `x-api-key`
    /// metadata.
    ///
    /// Starts at `start` initially; on every subsequent reconnect the resume
    /// point automatically advances to `last_block_height + 1` so fills are
    /// not lost across drops.
    pub fn new(endpoint: String, api_key: Option<String>, start: StartPosition) -> Self {
        let (tx, rx) = unbounded_channel();
        tokio::spawn(run(endpoint, api_key, start, tx));
        Self { rx }
    }

    /// Convenience: start from latest fills.
    pub fn latest(endpoint: String, api_key: Option<String>) -> Self {
        Self::new(endpoint, api_key, StartPosition::Latest)
    }

    /// Splits off the event stream (lets the caller drop the handle if
    /// convenient).
    pub fn into_stream(self) -> FillsConnectionStream {
        FillsConnectionStream { rx: self.rx }
    }
}

impl futures::Stream for FillsConnection {
    type Item = Event;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_recv(cx)
    }
}

impl futures::Stream for FillsConnectionStream {
    type Item = Event;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_recv(cx)
    }
}

// ---------------------------------------------------------------------------
// Reconnect loop.
// ---------------------------------------------------------------------------

async fn run(
    endpoint: String,
    api_key: Option<String>,
    start: StartPosition,
    tx: UnboundedSender<Event>,
) {
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const INITIAL_BACKOFF_MS: u64 = 500;
    const MAX_BACKOFF_MS: u64 = 5_000;

    let mut attempts: u32 = 0;
    let mut current_start = start;

    loop {
        let channel = match build_channel(&endpoint, CONNECT_TIMEOUT).await {
            Ok(ch) => ch,
            Err(err) => {
                log::error!("dwellir fills: channel to {endpoint} failed: {err:?}");
                backoff(&mut attempts, INITIAL_BACKOFF_MS, MAX_BACKOFF_MS).await;
                continue;
            }
        };

        let mut client = HyperliquidL1GatewayClient::new(channel);
        let mut request = current_start.to_request().into_request();
        if let Some(key) = &api_key {
            match MetadataValue::try_from(key.as_str()) {
                Ok(v) => {
                    request.metadata_mut().insert("x-api-key", v);
                }
                Err(err) => {
                    log::error!("dwellir fills: invalid API key header value: {err:?}");
                }
            }
        }

        let mut stream = match client.stream_fills(request).await {
            Ok(resp) => resp.into_inner(),
            Err(status) => {
                log::error!("dwellir fills: StreamFills rejected: {status}");
                backoff(&mut attempts, INITIAL_BACKOFF_MS, MAX_BACKOFF_MS).await;
                continue;
            }
        };

        attempts = 0;
        if tx.send(Event::Connected).is_err() {
            return;
        }

        loop {
            match stream.message().await {
                Ok(Some(raw)) => {
                    let block = match serde_json::from_slice::<FillsBlock>(&raw.data) {
                        Ok(b) => b,
                        Err(err) => {
                            log::warn!("dwellir fills: failed to parse block payload: {err:?}");
                            continue;
                        }
                    };
                    // Advance resume point so a future reconnect picks up from the next block.
                    current_start = StartPosition::BlockHeight(block.block_number as i64 + 1);
                    if tx.send(Event::Message(block)).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    log::warn!("dwellir fills: server closed stream");
                    break;
                }
                Err(status) => {
                    log::warn!("dwellir fills: stream error: {status}");
                    break;
                }
            }
        }

        if tx.send(Event::Disconnected).is_err() {
            return;
        }
        backoff(&mut attempts, INITIAL_BACKOFF_MS, MAX_BACKOFF_MS).await;
    }
}

pub(crate) async fn build_channel(
    endpoint: &str,
    connect_timeout: Duration,
) -> Result<Channel, anyhow::Error> {
    // rustls 0.23 requires a process-level crypto provider. `install_default`
    // is a no-op (returns Err) if one is already set, so this is safe to call
    // on every reconnect and from library code.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut ep = Endpoint::from_shared(endpoint.to_string())?.connect_timeout(connect_timeout);
    if endpoint.starts_with("https://") {
        ep = ep.tls_config(ClientTlsConfig::new().with_webpki_roots())?;
    }
    Ok(ep.connect().await?)
}

async fn backoff(attempts: &mut u32, initial_ms: u64, max_ms: u64) {
    let delay_ms = initial_ms
        .saturating_mul(1u64 << (*attempts).min(16))
        .min(max_ms);
    *attempts = attempts.saturating_add(1);
    log::debug!("dwellir fills: backoff {delay_ms}ms (attempt {attempts})");
    sleep(Duration::from_millis(delay_ms)).await;
}
