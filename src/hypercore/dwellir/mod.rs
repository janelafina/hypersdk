//! Dwellir integrations for Hyperliquid.
//!
//! Dwellir exposes an authenticated HTTP Info Endpoint plus two streams that
//! go beyond the native Hyperliquid WebSocket API:
//!
//! - **Info Endpoint** — authenticated HTTP JSON queries for user open orders
//!   and positions. See [`InfoClient`].
//! - **L4 order book** — per-order book data (individual orders, wallet
//!   addresses, order IDs). Delivered over WebSocket. See [`L4Connection`].
//! - **Fills** — real-time fills across the whole chain, delivered as a gRPC
//!   server-streaming RPC. See [`FillsConnection`].
//!
//! Both connections share the same event-oriented shape as the existing
//! [`crate::hypercore::ws::Connection`]: they yield `Event::Connected`,
//! `Event::Disconnected`, and `Event::Message(..)` values over a
//! `futures::Stream`, and reconnect automatically with exponential backoff
//! so they are safe to use in long-running processes.
//!
//! # Endpoints and credentials
//!
//! Endpoints are provided by the caller. For convenience the module reads
//! three environment variables (typically populated from a `.env`):
//!
//! | Variable               | Purpose                                                       |
//! |------------------------|---------------------------------------------------------------|
//! | `DWELLIR_WS_ENDPOINT`  | L4 WebSocket URL (often already contains an auth token).      |
//! | `DWELLIR_GRPC_ENDPOINT`| Fills gRPC endpoint, e.g. `https://hyperliquid.dwellir.com:443`. |
//! | `DWELLIR_API_KEY`      | API key for HTTP URL path and optional `x-api-key` gRPC metadata. |
//!
//! See [`info_from_env`], [`l4_from_env`], and [`fills_from_env`] for the
//! convenience wrappers.

pub mod grpc;
pub mod http;
pub mod types;
pub mod ws;

use std::env;

use anyhow::{Context, Result};
use url::Url;

pub use grpc::{
    Event as FillsEvent, FillsConnection, FillsConnectionStream, StartPosition as FillsStartPosition,
};
pub use http::{
    Client as InfoClient, DwellirInfoRequest, DwellirOpenOrder, DwellirPosition,
    DwellirPositionData, INFO_BASE_URL,
};
pub use types::*;
pub use ws::{
    Event as L4Event, L4Connection, L4ConnectionHandle, L4ConnectionStream,
};

/// Env var name for the Dwellir L4 WebSocket endpoint.
pub const WS_ENDPOINT_ENV: &str = "DWELLIR_WS_ENDPOINT";
/// Env var name for the Dwellir fills gRPC endpoint.
pub const GRPC_ENDPOINT_ENV: &str = "DWELLIR_GRPC_ENDPOINT";
/// Env var name for the Dwellir API key (sent as `x-api-key` gRPC metadata).
pub const API_KEY_ENV: &str = "DWELLIR_API_KEY";

/// Reads and parses [`WS_ENDPOINT_ENV`].
pub fn ws_endpoint_from_env() -> Result<Url> {
    let raw = env::var(WS_ENDPOINT_ENV)
        .with_context(|| format!("missing env var {WS_ENDPOINT_ENV}"))?;
    Url::parse(&raw).with_context(|| format!("invalid {WS_ENDPOINT_ENV}: {raw}"))
}

/// Reads [`GRPC_ENDPOINT_ENV`].
pub fn grpc_endpoint_from_env() -> Result<String> {
    env::var(GRPC_ENDPOINT_ENV)
        .with_context(|| format!("missing env var {GRPC_ENDPOINT_ENV}"))
}

/// Reads [`API_KEY_ENV`] if set.
pub fn api_key_from_env() -> Option<String> {
    env::var(API_KEY_ENV).ok()
}

/// Reads [`API_KEY_ENV`], returning an error if it is missing.
pub fn required_api_key_from_env() -> Result<String> {
    env::var(API_KEY_ENV).with_context(|| format!("missing env var {API_KEY_ENV}"))
}

/// Builds a Dwellir HTTP info client using [`API_KEY_ENV`].
pub fn info_from_env() -> Result<InfoClient> {
    Ok(InfoClient::new(required_api_key_from_env()?))
}

/// Builds an L4 WebSocket connection using [`WS_ENDPOINT_ENV`].
pub fn l4_from_env() -> Result<L4Connection> {
    Ok(L4Connection::new(ws_endpoint_from_env()?))
}

/// Builds a fills gRPC connection using [`GRPC_ENDPOINT_ENV`] and
/// [`API_KEY_ENV`], starting from the latest block.
pub fn fills_from_env() -> Result<FillsConnection> {
    Ok(FillsConnection::latest(
        grpc_endpoint_from_env()?,
        api_key_from_env(),
    ))
}

/// Builds a fills gRPC connection with a specific start position.
pub fn fills_from_env_at(start: FillsStartPosition) -> Result<FillsConnection> {
    Ok(FillsConnection::new(
        grpc_endpoint_from_env()?,
        api_key_from_env(),
        start,
    ))
}
