//! One interface over Dwellir's two order-book feeds.
//!
//! Downstream code that only needs "a book stream" can pick the transport at
//! runtime with [`BookSubscription`]:
//!
//! - [`BookSubscription::L4`]: per-order L4 book for one coin over the
//!   Dwellir WebSocket ([`L4Connection`]).
//! - [`BookSubscription::L2Diff`]: aggregated L2 book diffs for 1-20 coins
//!   over gRPC `StreamL2BookDiff` ([`L2BookDiffConnection`]).
//!
//! [`BookConnection`] yields [`BookEvent`]s with the same
//! `Connected` / `Disconnected` / `Message` shape as the underlying
//! connections. Messages are not normalized: L4 and L2 carry different
//! information (individual orders vs. aggregated levels), so
//! [`BookMessage`] keeps each payload in its native typed form.
//!
//! Recorders are deliberately left separate: use
//! [`L4BookRecorder`](super::L4BookRecorder) (which requires authoritative
//! snapshots with an exchange timestamp) for L4 and
//! [`L2BookSet`](super::L2BookSet) / [`L2BookRecorder`](super::L2BookRecorder)
//! for L2 diffs.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::StreamExt;

use super::{
    l2::{L2BookDiffConnection, L2BookDiffEvent, L2BookDiffRequest, L2BookDiffStreamError},
    types::{DwellirIncoming, DwellirSubscription, L2BookDiffUpdate, L4Message},
    ws::{Event as L4Event, L4Connection},
};

/// Which book feed to subscribe to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BookSubscription {
    /// Per-order L4 book for one coin (WebSocket).
    L4 { coin: String },
    /// Aggregated L2 book diffs for 1-20 coins (gRPC).
    L2Diff(L2BookDiffRequest),
}

impl BookSubscription {
    /// L4 book for `coin`.
    pub fn l4(coin: impl Into<String>) -> Self {
        Self::L4 { coin: coin.into() }
    }

    /// L2 diff stream for `request`.
    #[must_use]
    pub fn l2_diff(request: L2BookDiffRequest) -> Self {
        Self::L2Diff(request)
    }

    /// Coins covered by this subscription.
    #[must_use]
    pub fn coins(&self) -> Vec<&str> {
        match self {
            Self::L4 { coin } => vec![coin.as_str()],
            Self::L2Diff(request) => request.coins.iter().map(String::as_str).collect(),
        }
    }
}

/// Book payload from either feed.
// Unboxed to match `L4Event::Message(DwellirIncoming)`, which carries the same
// large L4 payload inline.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum BookMessage {
    /// L4 snapshot or update batch (for the subscribed coin).
    L4(L4Message),
    /// L2 diff frame (possibly covering several coins).
    L2(L2BookDiffUpdate),
}

/// Error reported by a [`BookConnection`].
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum BookError {
    /// Provider-reported WebSocket error string. Not terminal: the L4
    /// connection keeps running.
    #[error("L4 provider error: {0}")]
    L4Provider(String),
    /// Non-retryable gRPC failure. Terminal: the stream ends afterwards.
    #[error(transparent)]
    L2(#[from] L2BookDiffStreamError),
}

impl BookError {
    /// `true` when no further events will follow.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::L2(_))
    }
}

/// Lifecycle + data events yielded by a [`BookConnection`].
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum BookEvent {
    /// Transport connected (including after a reconnect). A fresh snapshot
    /// follows for every subscribed coin.
    Connected,
    /// Transport dropped; a reconnect is already being attempted.
    Disconnected,
    /// Book data.
    Message(BookMessage),
    /// Provider/stream error; see [`BookError::is_terminal`].
    Error(BookError),
}

enum Inner {
    L4(L4Connection),
    L2(L2BookDiffConnection),
}

/// Reconnecting book stream backed by either the L4 WebSocket or the L2
/// diff gRPC connection. Implements `futures::Stream<Item = BookEvent>`.
///
/// For the L4 variant the connection owns a dedicated WebSocket subscribed
/// only to `L4Book { coin }`; other WebSocket channels (trades,
/// subscription acknowledgements) are filtered out.
pub struct BookConnection {
    inner: Inner,
    subscription: BookSubscription,
}

impl BookConnection {
    /// Wraps a fresh L4 connection and subscribes it to `coin`.
    pub fn from_l4(connection: L4Connection, coin: impl Into<String>) -> Self {
        let coin = coin.into();
        connection.subscribe(DwellirSubscription::L4Book { coin: coin.clone() });
        Self {
            inner: Inner::L4(connection),
            subscription: BookSubscription::L4 { coin },
        }
    }

    /// Wraps an L2 diff connection.
    #[must_use]
    pub fn from_l2_diff(connection: L2BookDiffConnection) -> Self {
        let subscription = BookSubscription::L2Diff(connection.request().clone());
        Self {
            inner: Inner::L2(connection),
            subscription,
        }
    }

    /// The (validated) subscription this connection serves.
    #[must_use]
    pub fn subscription(&self) -> &BookSubscription {
        &self.subscription
    }

    /// Forces a reconnect and fresh snapshots after a detected gap.
    ///
    /// Supported for the L2 diff transport only; returns `false` (and does
    /// nothing) for L4, whose WebSocket has no resync request.
    pub fn reconnect(&self) -> bool {
        match &self.inner {
            Inner::L2(connection) => {
                connection.reconnect();
                true
            }
            Inner::L4(_) => false,
        }
    }
}

impl futures::Stream for BookConnection {
    type Item = BookEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            let event = match &mut this.inner {
                Inner::L2(connection) => {
                    return connection.poll_next_unpin(cx).map(|event| {
                        event.map(|event| match event {
                            L2BookDiffEvent::Connected => BookEvent::Connected,
                            L2BookDiffEvent::Disconnected => BookEvent::Disconnected,
                            L2BookDiffEvent::Message(update) => {
                                BookEvent::Message(BookMessage::L2(update))
                            }
                            L2BookDiffEvent::Error(err) => BookEvent::Error(BookError::L2(err)),
                        })
                    });
                }
                Inner::L4(connection) => match futures::ready!(connection.poll_next_unpin(cx)) {
                    None => return Poll::Ready(None),
                    Some(event) => event,
                },
            };
            let mapped = match event {
                L4Event::Connected => BookEvent::Connected,
                L4Event::Disconnected => BookEvent::Disconnected,
                L4Event::Message(DwellirIncoming::L4Book(message)) => {
                    BookEvent::Message(BookMessage::L4(message))
                }
                L4Event::Message(DwellirIncoming::Error(error)) => {
                    BookEvent::Error(BookError::L4Provider(error))
                }
                L4Event::Message(DwellirIncoming::Trades(_))
                | L4Event::Message(DwellirIncoming::SubscriptionResponse(_)) => continue,
            };
            return Poll::Ready(Some(mapped));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_coins_and_error_terminality() {
        assert_eq!(BookSubscription::l4("BTC").coins(), vec!["BTC"]);
        let sub = BookSubscription::l2_diff(L2BookDiffRequest::new(["BTC", "ETH"]));
        assert_eq!(sub.coins(), vec!["BTC", "ETH"]);
        assert!(!BookError::L4Provider("x".into()).is_terminal());
        assert!(BookError::L2(L2BookDiffStreamError::InvalidApiKey("x".into())).is_terminal());
    }
}
