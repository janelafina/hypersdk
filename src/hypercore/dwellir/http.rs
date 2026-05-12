//! HTTP JSON client for Dwellir's HyperCore Info Endpoint.
//!
//! Dwellir's info endpoint mirrors Hyperliquid info requests. For dedicated
//! nodes, the API key is embedded in the URL path:
//!
//! `https://dedicated-hyperliquid-...n.dwellir.com/{api_key}/info`
//!
//! This module currently exposes the user-centric queries needed alongside the
//! Dwellir streaming integrations: open orders, perpetual positions, and full
//! portfolio state.

use std::{collections::BTreeMap, time::Duration};

use alloy::primitives::Address;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::hypercore::types::{
    AssetPosition, ClearinghouseState, PositionData, SpotState, WsBasicOrder,
};

/// Default Dwellir Hyperliquid mainnet Info Endpoint base URL.
pub const INFO_BASE_URL: &str = "https://api-hyperliquid-mainnet-info.n.dwellir.com";

/// Dwellir `dex` value that requests native plus all operational HIP-3 DEXes.
pub const ALL_DEXES: &str = "ALL_DEXES";

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

/// Spot balances object returned inside Dwellir's `portfolioState` response.
pub type DwellirSpotState = SpotState;

/// Perpetual clearinghouse state returned by Dwellir's `portfolioState`.
///
/// The Dwellir wire shape changes with the requested `dex`:
/// - `ALL_DEXES` returns a map keyed by `"native"` plus HIP-3 DEX names.
/// - A specific DEX returns the regular single [`ClearinghouseState`] object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DwellirPortfolioClearinghouseState {
    Single(ClearinghouseState),
    ByDex(BTreeMap<String, ClearinghouseState>),
}

impl DwellirPortfolioClearinghouseState {
    /// Returns the regular single-DEX clearinghouse state, when that shape was returned.
    #[must_use]
    pub fn single(&self) -> Option<&ClearinghouseState> {
        match self {
            Self::Single(state) => Some(state),
            Self::ByDex(_) => None,
        }
    }

    /// Returns the DEX-keyed clearinghouse state map, when `ALL_DEXES` was returned.
    #[must_use]
    pub fn by_dex(&self) -> Option<&BTreeMap<String, ClearinghouseState>> {
        match self {
            Self::Single(_) => None,
            Self::ByDex(states) => Some(states),
        }
    }

    /// Returns the native DEX state.
    ///
    /// For a single-DEX response this returns that single state. For an `ALL_DEXES`
    /// response this returns the `"native"` entry.
    #[must_use]
    pub fn native(&self) -> Option<&ClearinghouseState> {
        match self {
            Self::Single(state) => Some(state),
            Self::ByDex(states) => states.get("native"),
        }
    }

    /// Returns the clearinghouse state for a DEX name in an `ALL_DEXES` response.
    #[must_use]
    pub fn dex(&self, dex_name: &str) -> Option<&ClearinghouseState> {
        match self {
            Self::Single(_) => None,
            Self::ByDex(states) => states.get(dex_name),
        }
    }
}

/// Full portfolio state returned by Dwellir's `portfolioState` info request.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DwellirPortfolioState {
    /// Perpetual clearinghouse state.
    pub clearinghouse_state: DwellirPortfolioClearinghouseState,
    /// Spot balances.
    pub spot_clearinghouse_state: DwellirSpotState,
    /// Account abstraction mode as returned by Dwellir.
    pub user_abstraction: String,
}

/// Request body for Dwellir's HyperCore Info Endpoint.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type")]
pub enum DwellirInfoRequest {
    /// `{ "type": "openOrders", "user": "0x..." }`
    OpenOrders { user: Address },
    /// `{ "type": "clearinghouseState", "user": "0x..." }`
    ClearinghouseState { user: Address },
    /// `{ "type": "portfolioState", "user": "0x...", "dex": "ALL_DEXES" }`
    PortfolioState {
        user: Address,
        #[serde(skip_serializing_if = "Option::is_none")]
        dex: Option<String>,
    },
}

/// Returns the API key embedded in a Dwellir dedicated-node endpoint path.
///
/// Dwellir dedicated WebSocket URLs commonly look like
/// `wss://dedicated-hyperliquid-...n.dwellir.com/{api_key}/ws`.
#[must_use]
pub fn api_key_from_endpoint_path(endpoint: &Url) -> Option<String> {
    endpoint
        .path_segments()?
        .find(|segment| !segment.is_empty() && *segment != "ws")
        .map(ToOwned::to_owned)
}

/// Normalizes a dedicated node host or endpoint into the HTTP info base URL.
///
/// Accepts either a bare host (`dedicated-hyperliquid-...n.dwellir.com`), an
/// HTTP(S) URL, or a WS(S) URL. Path, query, and fragment components are
/// removed because [`Client::info_url`] appends `/{api_key}/info`.
pub fn dedicated_node_info_base_url(endpoint_or_host: impl AsRef<str>) -> Result<Url> {
    let raw = endpoint_or_host.as_ref().trim();
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };

    let mut url = Url::parse(&with_scheme)
        .with_context(|| format!("invalid Dwellir dedicated node endpoint: {raw}"))?;
    let scheme = match url.scheme() {
        "wss" => "https",
        "ws" => "http",
        "https" => "https",
        "http" => "http",
        other => {
            return Err(anyhow!(
                "unsupported Dwellir dedicated node scheme: {other}"
            ));
        }
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow!("unable to set Dwellir dedicated node scheme to {scheme}"))?;
    url.set_path("");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// Async HTTP client for Dwellir's HyperCore Info Endpoint.
///
/// # Example
///
/// ```no_run
/// use hypersdk::{Address, hypercore::dwellir::InfoClient};
///
/// # async fn example() -> anyhow::Result<()> {
/// let client = InfoClient::for_dedicated_node(
///     "your-dwellir-api-key",
///     "dedicated-hyperliquid-tokyo-3.n.dwellir.com",
/// )?;
/// let user: Address = "0x0000000000000000000000000000000000000000".parse()?;
///
/// let orders = client.open_orders(user).await?;
/// let positions = client.positions(user).await?;
/// let _portfolio = client.portfolio_state(user, None).await?;
/// let _all_dexes = client.portfolio_state_all_dexes(user).await?;
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

    /// Creates a client for a Dwellir dedicated node.
    ///
    /// `dedicated_node_host` can be a bare host, an HTTP(S) base URL, or the
    /// same WS(S) endpoint used for the L4 stream.
    pub fn for_dedicated_node(
        api_key: impl Into<String>,
        dedicated_node_host: impl AsRef<str>,
    ) -> Result<Self> {
        Ok(Self::new(api_key).with_base_url(dedicated_node_info_base_url(dedicated_node_host)?))
    }

    /// Creates a dedicated-node info client by deriving both host and API key
    /// from a Dwellir WebSocket endpoint.
    pub fn from_ws_endpoint(ws_endpoint: &Url) -> Result<Self> {
        let api_key = api_key_from_endpoint_path(ws_endpoint).ok_or_else(|| {
            anyhow!("Dwellir WebSocket endpoint path does not contain an API key")
        })?;
        Self::for_dedicated_node(api_key, ws_endpoint.as_str())
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

    /// Returns the full portfolio state for `user`.
    ///
    /// If `dex` is `None`, the field is omitted and Dwellir returns the native
    /// Hyperliquid DEX. Pass a DEX name to query only that DEX, or pass
    /// [`ALL_DEXES`] to include every operational HIP-3 DEX.
    pub async fn portfolio_state(
        &self,
        user: Address,
        dex: Option<String>,
    ) -> Result<DwellirPortfolioState> {
        let req = DwellirInfoRequest::PortfolioState { user, dex };
        self.send_info_request("portfolio_state", &req).await
    }

    /// Returns the full portfolio state for the native DEX plus all operational HIP-3 DEXes.
    pub async fn portfolio_state_all_dexes(&self, user: Address) -> Result<DwellirPortfolioState> {
        self.portfolio_state(user, Some(ALL_DEXES.to_string())).await
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
    fn builds_dedicated_info_url_from_ws_endpoint() {
        let ws_endpoint =
            Url::parse("wss://dedicated-hyperliquid-tokyo-3.n.dwellir.com/secret-key/ws?x=1")
                .unwrap();
        let client = Client::from_ws_endpoint(&ws_endpoint).unwrap();
        assert_eq!(
            client.info_url().as_str(),
            "https://dedicated-hyperliquid-tokyo-3.n.dwellir.com/secret-key/info"
        );
    }

    #[test]
    fn builds_dedicated_info_url_from_bare_host() {
        let client =
            Client::for_dedicated_node("secret-key", "dedicated-hyperliquid-tokyo-3.n.dwellir.com")
                .unwrap();
        assert_eq!(
            client.info_url().as_str(),
            "https://dedicated-hyperliquid-tokyo-3.n.dwellir.com/secret-key/info"
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
    fn serializes_portfolio_state_request_shape() {
        let req = DwellirInfoRequest::PortfolioState {
            user: address!("0x000000000000000000000000000000000000dEaD"),
            dex: Some(ALL_DEXES.to_string()),
        };
        let json = serde_json::to_value(req).unwrap();
        assert_eq!(json["type"], "portfolioState");
        assert_eq!(json["user"], "0x000000000000000000000000000000000000dead");
        assert_eq!(json["dex"], ALL_DEXES);
    }

    #[test]
    fn serializes_portfolio_state_without_dex_when_omitted() {
        let req = DwellirInfoRequest::PortfolioState {
            user: address!("0x000000000000000000000000000000000000dEaD"),
            dex: None,
        };
        let json = serde_json::to_value(req).unwrap();
        assert_eq!(json["type"], "portfolioState");
        assert_eq!(json["user"], "0x000000000000000000000000000000000000dead");
        assert!(json.get("dex").is_none());
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

    #[test]
    fn parses_portfolio_state_all_dexes_shape() {
        let raw = r#"{
            "clearinghouseState": {
                "native": {
                    "marginSummary": {
                        "accountValue": "555.13644",
                        "totalMarginUsed": "0.0",
                        "totalNtlPos": "0.0",
                        "totalRawUsd": "555.13644"
                    },
                    "crossMarginSummary": {
                        "accountValue": "555.13644",
                        "totalMarginUsed": "0.0",
                        "totalNtlPos": "0.0",
                        "totalRawUsd": "555.13644"
                    },
                    "crossMaintenanceMarginUsed": "0.0",
                    "withdrawable": "555.13644",
                    "assetPositions": [],
                    "time": 1776448114752
                },
                "hip3Dex": {
                    "marginSummary": {
                        "accountValue": "0.0",
                        "totalMarginUsed": "0.0",
                        "totalNtlPos": "0.0",
                        "totalRawUsd": "0.0"
                    },
                    "crossMarginSummary": {
                        "accountValue": "0.0",
                        "totalMarginUsed": "0.0",
                        "totalNtlPos": "0.0",
                        "totalRawUsd": "0.0"
                    },
                    "crossMaintenanceMarginUsed": "0.0",
                    "withdrawable": "0.0",
                    "assetPositions": [],
                    "time": 1776448114649
                }
            },
            "spotClearinghouseState": {
                "balances": [
                    {
                        "coin": "USDC",
                        "token": 0,
                        "total": "12.27568764",
                        "hold": "0.0",
                        "entryNtl": "0.0"
                    }
                ]
            },
            "userAbstraction": "default"
        }"#;

        let state: DwellirPortfolioState = serde_json::from_str(raw).unwrap();
        assert_eq!(state.user_abstraction, "default");
        assert_eq!(state.spot_clearinghouse_state.balances.len(), 1);
        assert_eq!(
            state
                .clearinghouse_state
                .native()
                .unwrap()
                .margin_summary
                .account_value
                .to_string(),
            "555.13644"
        );
        assert!(state.clearinghouse_state.dex("hip3Dex").is_some());
        assert_eq!(state.clearinghouse_state.by_dex().unwrap().len(), 2);
    }

    #[test]
    fn parses_portfolio_state_single_dex_shape() {
        let raw = r#"{
            "clearinghouseState": {
                "marginSummary": {
                    "accountValue": "555.13644",
                    "totalMarginUsed": "0.0",
                    "totalNtlPos": "0.0",
                    "totalRawUsd": "555.13644"
                },
                "crossMarginSummary": {
                    "accountValue": "555.13644",
                    "totalMarginUsed": "0.0",
                    "totalNtlPos": "0.0",
                    "totalRawUsd": "555.13644"
                },
                "crossMaintenanceMarginUsed": "0.0",
                "withdrawable": "555.13644",
                "assetPositions": [],
                "time": 1776448114752
            },
            "spotClearinghouseState": {
                "balances": []
            },
            "userAbstraction": "unifiedAccount"
        }"#;

        let state: DwellirPortfolioState = serde_json::from_str(raw).unwrap();
        assert_eq!(state.user_abstraction, "unifiedAccount");
        assert!(state.clearinghouse_state.single().is_some());
        assert_eq!(
            state
                .clearinghouse_state
                .native()
                .unwrap()
                .withdrawable
                .to_string(),
            "555.13644"
        );
        assert!(state.clearinghouse_state.by_dex().is_none());
    }
}
