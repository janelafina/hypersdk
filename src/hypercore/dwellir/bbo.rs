//! gRPC client for Dwellir's `StreamBbo` endpoint.
//!
//! Dwellir's v3 `MarketStreaming` service
//! (`hyperliquid_l1_gateway.v3.MarketStreaming/StreamBbo`) streams the best
//! bid and ask of up to 20 coins on one RPC. It opens with the current top of
//! book of every coin, then sends one frame per coin whenever that coin's top
//! level (price, size or order count) changes. Each coin has its own order;
//! cross-coin order is unspecified. A frame may repeat a value already seen
//! (the server re-sends the top of book after recovering from subscriber lag),
//! and `ABORTED` means the stream state reset: reconnect for fresh opening
//! values. There is no resume cursor.
//!
//! The message layout (field numbers) was pinned from live frames on
//! 2026-09-26: `BboUpdate { coin = 1, time = 2, block_number = 3, bid = 4,
//! ask = 5 }` with the same `L2Level { px = 1, sz = 2, n = 3 }` as
//! `StreamL2BookDiff`.
//!
//! This module has no reconnect loop: [`stream_bbo`] opens one subscription and
//! the caller owns reconnection (every reconnect re-sends the opening values).

use std::{collections::HashSet, time::Duration};

use tonic::{
    IntoRequest, Response, Status, Streaming, client::Grpc, codec::ProstCodec,
    codegen::http::uri::PathAndQuery, metadata::MetadataValue,
};

use super::{
    grpc::build_channel,
    types::{L2DiffConversionError, convert_l2_level},
};
use crate::hypercore::types::{BookLevel, Side};

/// Maximum number of coins per `StreamBbo` subscription.
pub const BBO_MAX_COINS: usize = 20;

/// Server-streaming RPC path of `StreamBbo`.
pub const BBO_RPC_PATH: &str = "/hyperliquid_l1_gateway.v3.MarketStreaming/StreamBbo";

/// Raw protobuf messages of `hyperliquid_l1_gateway.v3` used by `StreamBbo`.
///
/// Prefer the typed [`Bbo`] and [`BboRequest`] in application code.
pub mod wire {
    pub use super::super::l2::wire::L2Level;

    /// `BboRequest`.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct BboRequest {
        /// 1-20 distinct, case-sensitive coins.
        #[prost(string, repeated, tag = "1")]
        pub coins: Vec<String>,
    }

    /// `BboUpdate`: the top of book of one coin.
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct BboUpdate {
        #[prost(string, tag = "1")]
        pub coin: String,
        /// Block time, Unix milliseconds.
        #[prost(int64, tag = "2")]
        pub time: i64,
        #[prost(int64, tag = "3")]
        pub block_number: i64,
        /// Unset when the bid side is empty.
        #[prost(message, optional, tag = "4")]
        pub bid: Option<L2Level>,
        /// Unset when the ask side is empty.
        #[prost(message, optional, tag = "5")]
        pub ask: Option<L2Level>,
    }
}

/// Subscription parameters for `StreamBbo`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BboRequest {
    /// Case-sensitive native coins (`BTC`, `xyz:XYZ100`), 1-20 distinct values.
    pub coins: Vec<String>,
}

/// Client-side validation failure for a [`BboRequest`], mirroring the
/// server's `INVALID_ARGUMENT` contract.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum BboRequestError {
    #[error("at least one coin is required")]
    NoCoins,
    #[error("too many coins: {0} distinct (max {BBO_MAX_COINS})")]
    TooManyCoins(usize),
    #[error("coin names must be non-blank and not wildcards, got {0:?}")]
    InvalidCoin(String),
}

impl BboRequest {
    /// Request for `coins`.
    pub fn new<I, S>(coins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            coins: coins.into_iter().map(Into::into).collect(),
        }
    }

    /// Validates the request and returns a copy with duplicate coins removed
    /// (order of first occurrence preserved).
    pub fn validate(&self) -> Result<Self, BboRequestError> {
        let mut seen = HashSet::new();
        let mut coins = Vec::with_capacity(self.coins.len());
        for coin in &self.coins {
            if coin.trim().is_empty() || coin.contains('*') {
                return Err(BboRequestError::InvalidCoin(coin.clone()));
            }
            if seen.insert(coin.as_str()) {
                coins.push(coin.clone());
            }
        }
        if coins.is_empty() {
            return Err(BboRequestError::NoCoins);
        }
        if coins.len() > BBO_MAX_COINS {
            return Err(BboRequestError::TooManyCoins(coins.len()));
        }
        Ok(Self { coins })
    }

    /// The protobuf request.
    #[must_use]
    pub fn to_wire(&self) -> wire::BboRequest {
        wire::BboRequest {
            coins: self.coins.clone(),
        }
    }
}

/// The best bid and ask of one coin at one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bbo {
    pub coin: String,
    /// Block time, Unix milliseconds.
    pub time: u64,
    pub block_number: u64,
    /// `None` when the bid side is empty.
    pub bid: Option<BookLevel>,
    /// `None` when the ask side is empty.
    pub ask: Option<BookLevel>,
}

impl TryFrom<wire::BboUpdate> for Bbo {
    type Error = L2DiffConversionError;

    fn try_from(raw: wire::BboUpdate) -> Result<Self, Self::Error> {
        if raw.coin.is_empty() {
            return Err(L2DiffConversionError::EmptyCoin);
        }
        let time = u64::try_from(raw.time).map_err(|_| L2DiffConversionError::NegativeField {
            field: "time",
            value: raw.time,
        })?;
        let block_number =
            u64::try_from(raw.block_number).map_err(|_| L2DiffConversionError::NegativeField {
                field: "block_number",
                value: raw.block_number,
            })?;
        let bid = raw
            .bid
            .map(|level| convert_l2_level(&raw.coin, Side::Bid, level))
            .transpose()?;
        let ask = raw
            .ask
            .map(|level| convert_l2_level(&raw.coin, Side::Ask, level))
            .transpose()?;
        Ok(Self {
            coin: raw.coin,
            time,
            block_number,
            bid,
            ask,
        })
    }
}

/// Failure opening a `StreamBbo` subscription.
#[derive(Debug, thiserror::Error)]
pub enum BboStreamError {
    #[error(transparent)]
    Request(#[from] BboRequestError),
    #[error("invalid API key metadata")]
    InvalidApiKey,
    #[error("connect: {0}")]
    Connect(anyhow::Error),
    /// The server refused the subscription (for example `INVALID_ARGUMENT`
    /// for an unknown coin, `FAILED_PRECONDITION` where the stream is off).
    #[error("StreamBbo refused: {0}")]
    Status(#[from] Status),
}

/// Opens one `StreamBbo` subscription on `endpoint` (`https://<node>:443`)
/// with optional `x-api-key` metadata. Every frame decodes to a
/// [`wire::BboUpdate`]; convert with [`Bbo::try_from`].
pub async fn stream_bbo(
    endpoint: &str,
    api_key: Option<&str>,
    request: &BboRequest,
) -> Result<Response<Streaming<wire::BboUpdate>>, BboStreamError> {
    let request = request.validate()?;
    let channel = build_channel(endpoint, Duration::from_secs(10))
        .await
        .map_err(BboStreamError::Connect)?;
    let mut client = Grpc::new(channel);
    client
        .ready()
        .await
        .map_err(|error| BboStreamError::Connect(error.into()))?;
    let mut grpc_request = request.to_wire().into_request();
    if let Some(key) = api_key {
        let key = MetadataValue::try_from(key).map_err(|_| BboStreamError::InvalidApiKey)?;
        grpc_request.metadata_mut().insert("x-api-key", key);
    }
    let codec: ProstCodec<wire::BboRequest, wire::BboUpdate> = ProstCodec::default();
    Ok(client
        .server_streaming(grpc_request, PathAndQuery::from_static(BBO_RPC_PATH), codec)
        .await?)
}

#[cfg(test)]
mod tests {
    use prost::Message as _;
    use rust_decimal::Decimal;

    use super::*;

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn bytes_field(tag: u8, value: &[u8], out: &mut Vec<u8>) {
        out.push(tag);
        varint(value.len() as u64, out);
        out.extend_from_slice(value);
    }

    fn level(px: &str, sz: &str, n: u64) -> Vec<u8> {
        let mut out = Vec::new();
        bytes_field(0x0a, px.as_bytes(), &mut out);
        bytes_field(0x12, sz.as_bytes(), &mut out);
        out.push(0x18);
        varint(n, &mut out);
        out
    }

    /// The field layout of a frame received from the live endpoint on
    /// 2026-09-26 (CASHCAT), written tag by tag.
    fn live_frame() -> Vec<u8> {
        let mut frame = Vec::new();
        bytes_field(0x0a, b"CASHCAT", &mut frame);
        frame.push(0x10);
        varint(1_790_380_731_676, &mut frame);
        frame.push(0x18);
        varint(1_161_155_112, &mut frame);
        bytes_field(0x22, &level("0.19549", "1079", 1), &mut frame);
        bytes_field(0x2a, &level("0.19561", "250", 1), &mut frame);
        frame
    }

    #[test]
    fn decodes_the_pinned_layout() {
        let raw = wire::BboUpdate::decode(live_frame().as_slice()).expect("decodes");
        let bbo = Bbo::try_from(raw).expect("converts");
        assert_eq!(bbo.coin, "CASHCAT");
        assert_eq!(bbo.time, 1_790_380_731_676);
        assert_eq!(bbo.block_number, 1_161_155_112);
        let bid = bbo.bid.expect("bid");
        assert_eq!(bid.px, "0.19549".parse::<Decimal>().expect("decimal"));
        assert_eq!(bid.sz, Decimal::from(1079));
        assert_eq!(bid.n, 1);
        assert_eq!(
            bbo.ask.expect("ask").px,
            "0.19561".parse::<Decimal>().expect("decimal")
        );
    }

    #[test]
    fn an_empty_side_is_none() {
        let raw = wire::BboUpdate {
            coin: "BTC".into(),
            time: 1,
            block_number: 2,
            bid: None,
            ask: Some(wire::L2Level {
                px: "1".into(),
                sz: "1".into(),
                n: 1,
            }),
        };
        let bbo = Bbo::try_from(raw).expect("converts");
        assert!(bbo.bid.is_none());
        assert!(bbo.ask.is_some());
    }

    #[test]
    fn requests_follow_the_server_contract() {
        let request = BboRequest::new(["BTC", "ETH", "BTC"])
            .validate()
            .expect("valid");
        assert_eq!(request.coins, ["BTC", "ETH"]);
        assert_eq!(
            BboRequest::new(Vec::<String>::new()).validate(),
            Err(BboRequestError::NoCoins)
        );
        assert!(matches!(
            BboRequest::new([" "]).validate(),
            Err(BboRequestError::InvalidCoin(_))
        ));
        assert!(matches!(
            BboRequest::new(["*"]).validate(),
            Err(BboRequestError::InvalidCoin(_))
        ));
        let many: Vec<String> = (0..21).map(|index| format!("C{index}")).collect();
        assert_eq!(
            BboRequest::new(many).validate(),
            Err(BboRequestError::TooManyCoins(21))
        );
        let twenty_with_duplicates: Vec<String> =
            (0..25).map(|index| format!("C{}", index % 20)).collect();
        assert_eq!(
            BboRequest::new(twenty_with_duplicates)
                .validate()
                .expect("valid")
                .coins
                .len(),
            20
        );
    }
}
