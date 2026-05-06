//! HTTP JSON client for Dwellir's HyperCore Info Endpoint.
//!
//! Dwellir's info endpoint mirrors Hyperliquid info requests, but the API key
//! is embedded in the URL path:
//!
//! `https://api-hyperliquid-mainnet-info.n.dwellir.com/{api_key}/info`
//!
//! This module currently exposes the user-centric queries needed alongside the
//! Dwellir streaming integrations: open orders and perpetual positions.

use std::time::Duration;

use alloy::primitives::Address;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::hypercore::types::{AssetPosition, ClearinghouseState, PositionData, WsBasicOrder};

/// Default Dwellir Hyperliquid mainnet Info Endpoint base URL.
pub const INFO_BASE_URL: &str = "https://api-hyperliquid-mainnet-info.n.dwellir.com";

/// Open-order shape returned by Dwellir's `openOrders` info request.
///
/// This is the same compact order shape used by native WebSocket order updates:
/// `coin`, `side`, `limitPx`, `sz`, `oid`, `timestamp`, `origSz`, and optional
/// `cloid`.
pub type DwellirOpenOrder = WsBasicOrder;

/// Asset-position shape returned inside `clearinghouseState.assetPositions`.
pub type DwellirPosition = AssetPosition;

/// Inner position data for a [`DwellirPosition`].
pub type DwellirPositionData = PositionData;

/// Request body for Dwellir's HyperCore Info Endpoint.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type")]
pub enum DwellirInfoRequest {
    /// `{ "type": "openOrders", "user": "0x..." }`
    OpenOrders { user: Address },
    /// `{ "type": "clearinghouseState", "user": "0x..." }`
    ClearinghouseState { user: Address },
}

/// Async HTTP client for Dwellir's HyperCore Info Endpoint.
///
/// # Example
///
/// ```no_run
/// use hypersdk::{Address, hypercore::dwellir::InfoClient};
///
/// # async fn example() -> anyhow::Result<()> {
/// let client = InfoClient::new("your-dwellir-api-key");
/// let user: Address = "0x0000000000000000000000000000000000000000".parse()?;
///
/// let orders = client.open_orders(user).await?;
/// let positions = client.positions(user).await?;
/// # Ok(())
/// # }
/// ```
pub struct Client {
    http_client: reqwest::Client,
    base_url: Url,
    api_key: String,
}

impl Client {
    /// Creates a client for Dwellir's default Hyperliquid mainnet info endpoint.
    pub fn new(api_key: impl Into<String>) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .tcp_nodelay(true)
            .build()
            .unwrap();

        Self {
            http_client,
            base_url: Url::parse(INFO_BASE_URL).expect("valid Dwellir info base URL"),
            api_key: api_key.into(),
        }
    }

    /// Sets a custom base URL while preserving the API-key path layout.
    ///
    /// The final request URL is always `{base_url}/{api_key}/info`.
    /// Useful for tests, proxies, or future Dwellir endpoint variants.
    #[must_use]
    pub fn with_base_url(self, base_url: Url) -> Self {
        Self { base_url, ..self }
    }

    /// Sets a custom [`reqwest::Client`] for HTTP requests.
    #[must_use]
    pub fn with_http_client(self, http_client: reqwest::Client) -> Self {
        Self {
            http_client,
            ..self
        }
    }

    /// Returns the configured Dwellir API key.
    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Returns the Dwellir info endpoint URL used for requests.
    #[must_use]
    pub fn info_url(&self) -> Url {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("Dwellir info base URL must support path segments")
            .clear()
            .push(&self.api_key)
            .push("info");
        url
    }

    /// Send an info request and deserialize the JSON response.
    async fn send_info_request<R>(&self, label: &str, req: &impl Serialize) -> Result<R>
    where
        R: for<'de> Deserialize<'de>,
    {
        let res = self
            .http_client
            .post(self.info_url())
            .json(&req)
            .send()
            .await?;
        let status = res.status();
        let bytes = res.bytes().await?;
        let text = String::from_utf8_lossy(&bytes);

        if !status.is_success() {
            return Err(anyhow!("[dwellir::{label}] HTTP {status} body={text}"));
        }

        serde_json::from_str(&text)
            .map_err(|e| anyhow!("[dwellir::{label}] decode failed: {e}; body={text}"))
    }

    /// Returns all open orders for `user` via Dwellir's `openOrders` info request.
    pub async fn open_orders(&self, user: Address) -> Result<Vec<DwellirOpenOrder>> {
        let req = DwellirInfoRequest::OpenOrders { user };
        self.send_info_request("open_orders", &req).await
    }

    /// Returns the complete perpetual clearinghouse state for `user`.
    pub async fn clearinghouse_state(&self, user: Address) -> Result<ClearinghouseState> {
        let req = DwellirInfoRequest::ClearinghouseState { user };
        self.send_info_request("clearinghouse_state", &req).await
    }

    /// Returns perpetual asset positions for `user`.
    ///
    /// This is derived from `clearinghouseState.assetPositions`.
    pub async fn positions(&self, user: Address) -> Result<Vec<DwellirPosition>> {
        Ok(self.clearinghouse_state(user).await?.asset_positions)
    }

    /// Alias for [`Self::positions`] that matches the wire field name.
    pub async fn asset_positions(&self, user: Address) -> Result<Vec<DwellirPosition>> {
        self.positions(user).await
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;

    #[test]
    fn builds_info_url_with_api_key_in_path() {
        let client = Client::new("secret-key");
        assert_eq!(
            client.info_url().as_str(),
            "https://api-hyperliquid-mainnet-info.n.dwellir.com/secret-key/info"
        );
    }

    #[test]
    fn serializes_open_orders_request_shape() {
        let req = DwellirInfoRequest::OpenOrders {
            user: address!("0x000000000000000000000000000000000000dEaD"),
        };
        let json = serde_json::to_value(req).unwrap();
        assert_eq!(json["type"], "openOrders");
        assert_eq!(json["user"], "0x000000000000000000000000000000000000dead");
    }

    #[test]
    fn serializes_clearinghouse_state_request_shape() {
        let req = DwellirInfoRequest::ClearinghouseState {
            user: address!("0x000000000000000000000000000000000000dEaD"),
        };
        let json = serde_json::to_value(req).unwrap();
        assert_eq!(json["type"], "clearinghouseState");
        assert_eq!(json["user"], "0x000000000000000000000000000000000000dead");
    }

    #[test]
    fn parses_dwellir_open_orders_with_optional_cloid() {
        let raw = r#"[
            {
                "coin": "BTC",
                "side": "B",
                "limitPx": "65000.5",
                "sz": "0.01",
                "oid": 12345,
                "timestamp": 1700000000000,
                "origSz": "0.02",
                "cloid": "0x00000000000000000000000000000001"
            },
            {
                "coin": "ETH",
                "side": "A",
                "limitPx": "3500",
                "sz": "1.5",
                "oid": 67890,
                "timestamp": 1700000000001,
                "origSz": "1.5"
            }
        ]"#;

        let orders: Vec<DwellirOpenOrder> = serde_json::from_str(raw).unwrap();
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0].coin, "BTC");
        assert_eq!(orders[0].limit_px.to_string(), "65000.5");
        assert!(orders[0].cloid.is_some());
        assert_eq!(orders[1].coin, "ETH");
        assert!(orders[1].cloid.is_none());
    }

    #[test]
    fn parses_clearinghouse_state_asset_positions() {
        let raw = r#"{
            "marginSummary": {
                "accountValue": "1000.0",
                "totalNtlPos": "500.0",
                "totalRawUsd": "1000.0",
                "totalMarginUsed": "50.0"
            },
            "crossMarginSummary": {
                "accountValue": "1000.0",
                "totalNtlPos": "500.0",
                "totalRawUsd": "1000.0",
                "totalMarginUsed": "50.0"
            },
            "crossMaintenanceMarginUsed": "10.0",
            "withdrawable": "900.0",
            "assetPositions": [{
                "type": "oneWay",
                "position": {
                    "coin": "BTC",
                    "szi": "0.01",
                    "leverage": { "type": "cross", "value": 20 },
                    "entryPx": "60000.0",
                    "positionValue": "650.0",
                    "unrealizedPnl": "50.0",
                    "returnOnEquity": "0.10",
                    "liquidationPx": null,
                    "marginUsed": "32.5",
                    "maxLeverage": 50,
                    "cumFunding": {
                        "allTime": "1.0",
                        "sinceOpen": "0.5",
                        "sinceChange": "0.25"
                    }
                }
            }],
            "time": 1700000000000
        }"#;

        let state: ClearinghouseState = serde_json::from_str(raw).unwrap();
        assert_eq!(state.asset_positions.len(), 1);
        let position = &state.asset_positions[0].position;
        assert_eq!(position.coin, "BTC");
        assert!(position.is_long());
        assert_eq!(position.entry_px.unwrap().to_string(), "60000.0");
    }
}
