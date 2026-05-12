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
//! Endpoints are derived from the caller's dedicated node host and API key. For
//! convenience the module reads environment variables typically populated from
//! a `.env`:
//!
//! | Variable               | Purpose                                                       |
//! |------------------------|---------------------------------------------------------------|
//! | `DWELLIR_NODE_HOST`    | Dedicated node host, e.g. `dedicated-hyperliquid-...n.dwellir.com`. |
//! | `DWELLIR_API_KEY`      | API key used in REST/WS paths and optional `x-api-key` gRPC metadata. |
//! | `DWELLIR_WS_ENDPOINT`  | Backward-compatible fallback; full L4 WebSocket URL.          |
//! | `DWELLIR_GRPC_ENDPOINT`| Backward-compatible fallback for the fills gRPC endpoint.     |
//!
//! [`Config`] lets callers configure the dedicated node once and then derive
//! the L4 WebSocket endpoint, fills gRPC endpoint, and HTTP info client from it.
//! See [`Config::from_env`], [`info_from_env`], [`l4_from_env`], and
//! [`fills_from_env`] for convenience wrappers.

pub mod grpc;
pub mod http;
pub mod types;
pub mod ws;

use std::env;

use anyhow::{Context, Result, anyhow};
use url::Url;

pub use grpc::{
    Event as FillsEvent, FillsConnection, FillsConnectionStream,
    StartPosition as FillsStartPosition,
};
pub use http::{
    ALL_DEXES, Client as InfoClient, DwellirInfoRequest, DwellirOpenOrder,
    DwellirPortfolioClearinghouseState, DwellirPortfolioState, DwellirPosition,
    DwellirPositionData, DwellirSpotState, INFO_BASE_URL, api_key_from_endpoint_path,
    dedicated_node_info_base_url,
};
pub use types::*;
pub use ws::{Event as L4Event, L4Connection, L4ConnectionHandle, L4ConnectionStream};

/// Env var name for the Dwellir L4 WebSocket endpoint.
pub const WS_ENDPOINT_ENV: &str = "DWELLIR_WS_ENDPOINT";
/// Env var name for the Dwellir dedicated node host.
pub const NODE_HOST_ENV: &str = "DWELLIR_NODE_HOST";
/// Env var name for the Dwellir fills gRPC endpoint.
pub const GRPC_ENDPOINT_ENV: &str = "DWELLIR_GRPC_ENDPOINT";
/// Env var name for the Dwellir API key (sent as `x-api-key` gRPC metadata).
pub const API_KEY_ENV: &str = "DWELLIR_API_KEY";

/// Dwellir dedicated-node SDK configuration.
///
/// Use this when you want to configure the dedicated node endpoint once and
/// build multiple Dwellir clients from the same host and API key.
#[derive(Debug, Clone)]
pub struct Config {
    node_host: String,
    api_key: String,
}

impl Config {
    /// Creates config from a Dwellir dedicated node host and API key.
    pub fn new(node_host: impl AsRef<str>, api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            node_host: dedicated_node_host(node_host)?,
            api_key: api_key.into(),
        })
    }

    /// Creates config from a Dwellir dedicated-node WebSocket endpoint.
    ///
    /// This is retained as a compatibility path for callers that already store
    /// `wss://{host}/{api_key}/ws`.
    pub fn from_ws_endpoint(ws_endpoint: Url) -> Result<Self> {
        let node_host = ws_endpoint
            .host_str()
            .ok_or_else(|| anyhow!("Dwellir WebSocket endpoint is missing a host"))?;
        let api_key = api_key_from_endpoint_path(&ws_endpoint).ok_or_else(|| {
            anyhow!("Dwellir WebSocket endpoint path does not contain an API key")
        })?;
        Self::new(node_host, api_key)
    }

    /// Reads [`NODE_HOST_ENV`] and [`API_KEY_ENV`].
    ///
    /// If [`NODE_HOST_ENV`] is not set, this falls back to deriving both values
    /// from [`WS_ENDPOINT_ENV`] for compatibility.
    pub fn from_env() -> Result<Self> {
        if let Ok(node_host) = env::var(NODE_HOST_ENV) {
            return Self::new(
                node_host,
                env::var(API_KEY_ENV).with_context(|| format!("missing env var {API_KEY_ENV}"))?,
            );
        }

        let ws_endpoint = ws_endpoint_from_env()?;
        let mut config = Self::from_ws_endpoint(ws_endpoint)?;
        if let Some(api_key) = api_key_from_env() {
            config.api_key = api_key;
        }
        Ok(config)
    }

    /// Overrides the API key used for HTTP info and gRPC metadata.
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = api_key.into();
        self
    }

    /// Returns the configured dedicated node host.
    #[must_use]
    pub fn node_host(&self) -> &str {
        &self.node_host
    }

    /// Returns the derived dedicated-node WebSocket endpoint.
    #[must_use]
    pub fn ws_endpoint(&self) -> Url {
        Url::parse(&format!("wss://{}/{}/ws", self.node_host, self.api_key))
            .expect("valid derived Dwellir WebSocket endpoint")
    }

    /// Returns the derived fills gRPC endpoint.
    #[must_use]
    pub fn grpc_endpoint(&self) -> String {
        if self.node_host.contains(':') {
            format!("https://{}", self.node_host)
        } else {
            format!("https://{}:443", self.node_host)
        }
    }

    /// Returns the derived HTTP info endpoint.
    #[must_use]
    pub fn info_url(&self) -> Url {
        InfoClient::for_dedicated_node(self.api_key.clone(), &self.node_host)
            .expect("valid Dwellir info client from config")
            .info_url()
    }

    /// Returns the configured API key.
    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Builds an HTTP info client for the configured dedicated node.
    pub fn info_client(&self) -> Result<InfoClient> {
        InfoClient::for_dedicated_node(self.api_key.clone(), &self.node_host)
    }

    /// Builds an L4 WebSocket connection for the configured dedicated node.
    #[must_use]
    pub fn l4_connection(&self) -> L4Connection {
        L4Connection::new(self.ws_endpoint())
    }

    /// Builds a fills gRPC connection for the configured dedicated node.
    #[must_use]
    pub fn fills_connection(&self) -> FillsConnection {
        FillsConnection::latest(self.grpc_endpoint(), Some(self.api_key.clone()))
    }
}

fn dedicated_node_host(endpoint_or_host: impl AsRef<str>) -> Result<String> {
    let base_url = dedicated_node_info_base_url(endpoint_or_host)?;
    let host = base_url
        .host_str()
        .ok_or_else(|| anyhow!("Dwellir dedicated node host is missing"))?;
    if let Some(port) = base_url.port() {
        Ok(format!("{host}:{port}"))
    } else {
        Ok(host.to_string())
    }
}

/// Reads and parses [`WS_ENDPOINT_ENV`].
pub fn ws_endpoint_from_env() -> Result<Url> {
    let raw =
        env::var(WS_ENDPOINT_ENV).with_context(|| format!("missing env var {WS_ENDPOINT_ENV}"))?;
    Url::parse(&raw).with_context(|| format!("invalid {WS_ENDPOINT_ENV}: {raw}"))
}

/// Reads [`GRPC_ENDPOINT_ENV`].
pub fn grpc_endpoint_from_env() -> Result<String> {
    env::var(GRPC_ENDPOINT_ENV).with_context(|| format!("missing env var {GRPC_ENDPOINT_ENV}"))
}

/// Reads [`API_KEY_ENV`] if set.
pub fn api_key_from_env() -> Option<String> {
    env::var(API_KEY_ENV).ok()
}

/// Reads [`API_KEY_ENV`], returning an error if it is missing.
pub fn required_api_key_from_env() -> Result<String> {
    env::var(API_KEY_ENV).with_context(|| format!("missing env var {API_KEY_ENV}"))
}

/// Builds a Dwellir HTTP info client using [`NODE_HOST_ENV`] and [`API_KEY_ENV`].
///
/// Falls back to deriving both values from [`WS_ENDPOINT_ENV`] for compatibility.
pub fn info_from_env() -> Result<InfoClient> {
    Config::from_env()?.info_client()
}

/// Builds an L4 WebSocket connection using [`NODE_HOST_ENV`] and [`API_KEY_ENV`].
pub fn l4_from_env() -> Result<L4Connection> {
    Ok(Config::from_env()?.l4_connection())
}

/// Builds a fills gRPC connection using [`NODE_HOST_ENV`] and [`API_KEY_ENV`],
/// starting from the latest block.
///
/// Falls back to [`GRPC_ENDPOINT_ENV`] plus [`API_KEY_ENV`] when
/// [`NODE_HOST_ENV`] is not configured.
pub fn fills_from_env() -> Result<FillsConnection> {
    if env::var(NODE_HOST_ENV).is_ok() {
        return Ok(Config::from_env()?.fills_connection());
    }

    Ok(FillsConnection::latest(
        grpc_endpoint_from_env()?,
        api_key_from_env(),
    ))
}

/// Builds a fills gRPC connection with a specific start position.
pub fn fills_from_env_at(start: FillsStartPosition) -> Result<FillsConnection> {
    if env::var(NODE_HOST_ENV).is_ok() {
        let config = Config::from_env()?;
        return Ok(FillsConnection::new(
            config.grpc_endpoint(),
            Some(config.api_key().to_string()),
            start,
        ));
    }

    Ok(FillsConnection::new(
        grpc_endpoint_from_env()?,
        api_key_from_env(),
        start,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_derives_dedicated_endpoints_from_host_and_key() {
        let config =
            Config::new("dedicated-hyperliquid-tokyo-3.n.dwellir.com", "secret-key").unwrap();

        assert_eq!(
            config.node_host(),
            "dedicated-hyperliquid-tokyo-3.n.dwellir.com"
        );
        assert_eq!(
            config.ws_endpoint().as_str(),
            "wss://dedicated-hyperliquid-tokyo-3.n.dwellir.com/secret-key/ws"
        );
        assert_eq!(
            config.info_url().as_str(),
            "https://dedicated-hyperliquid-tokyo-3.n.dwellir.com/secret-key/info"
        );
        assert_eq!(
            config.grpc_endpoint(),
            "https://dedicated-hyperliquid-tokyo-3.n.dwellir.com:443"
        );
    }

    #[test]
    fn config_can_be_derived_from_legacy_ws_endpoint() {
        let ws_endpoint =
            Url::parse("wss://dedicated-hyperliquid-tokyo-3.n.dwellir.com/secret-key/ws").unwrap();
        let config = Config::from_ws_endpoint(ws_endpoint).unwrap();

        assert_eq!(
            config.node_host(),
            "dedicated-hyperliquid-tokyo-3.n.dwellir.com"
        );
        assert_eq!(config.api_key(), "secret-key");
    }
}
