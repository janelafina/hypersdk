//! Reconnecting WebSocket client for Dwellir's L4 order-book feed.
//!
//! Mirrors [`crate::hypercore::ws::Connection`] in shape and guarantees:
//! - Exponential backoff on connection failure.
//! - Active subscriptions are replayed automatically after a reconnect.
//! - [`Event::Connected`] / [`Event::Disconnected`] lifecycle events are
//!   emitted on the stream so consumers can track health.
//! - Standard WebSocket control-frame ping/pong is handled by the underlying
//!   transport; the Dwellir L4 feed does not define JSON-level heartbeats.

use std::{
    collections::HashSet,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    time::{sleep, timeout},
};
use url::Url;
use yawc::{Frame, OpCode, Options, TcpWebSocket};

use super::types::{DwellirIncoming, DwellirOutgoing, DwellirSubscription};

struct Stream {
    stream: TcpWebSocket,
}

impl Stream {
    async fn connect(url: Url) -> Result<Self> {
        // L4 snapshots are large (BTC easily > 10 MB). Default yawc limits are
        // 1 MB payload / 2 MB buffer — those drop the connection mid-snapshot.
        // Match Dwellir's own Go example default.
        const MAX_PAYLOAD: usize = 150 * 1024 * 1024;
        const MAX_BUFFER: usize = 150 * 1024 * 1024;
        let stream = yawc::WebSocket::connect(url)
            .with_options(
                Options::default()
                    .with_no_delay()
                    .with_balanced_compression()
                    .with_utf8()
                    .with_limits(MAX_PAYLOAD, MAX_BUFFER),
            )
            .await?;
        Ok(Self { stream })
    }

    async fn subscribe(&mut self, subscription: DwellirSubscription) -> Result<()> {
        let text = serde_json::to_string(&DwellirOutgoing::Subscribe { subscription })?;
        self.stream.send(Frame::text(text)).await?;
        Ok(())
    }

    async fn unsubscribe(&mut self, subscription: DwellirSubscription) -> Result<()> {
        let text = serde_json::to_string(&DwellirOutgoing::Unsubscribe { subscription })?;
        self.stream.send(Frame::text(text)).await?;
        Ok(())
    }
}

impl futures::Stream for Stream {
    type Item = DwellirIncoming;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            let frame = match futures::ready!(this.stream.poll_next_unpin(cx)) {
                Some(f) => f,
                None => return Poll::Ready(None),
            };
            if frame.opcode() == OpCode::Text {
                match serde_json::from_slice::<DwellirIncoming>(frame.payload()) {
                    Ok(msg) => return Poll::Ready(Some(msg)),
                    Err(err) => {
                        log::warn!(
                            "dwellir L4: unable to parse frame: {:?} (payload len {})",
                            err,
                            frame.payload().len()
                        );
                    }
                }
            } else {
                log::trace!(
                    "dwellir L4: non-text frame (opcode={:?}, {} bytes)",
                    frame.opcode(),
                    frame.payload().len()
                );
            }
        }
    }
}

type SubChannelData = (bool, DwellirSubscription);

/// Lifecycle + data events yielded by an [`L4Connection`].
#[derive(Clone, Debug)]
pub enum Event {
    /// Connection established (including after a reconnect). Subscriptions
    /// are replayed immediately before this is emitted.
    Connected,
    /// Connection dropped; reconnect is already being attempted.
    Disconnected,
    /// A parsed message from the server.
    Message(DwellirIncoming),
}

/// Reconnecting WebSocket connection to a Dwellir L4 endpoint.
///
/// Implements `futures::Stream<Item = Event>`.
pub struct L4Connection {
    rx: UnboundedReceiver<Event>,
    tx: UnboundedSender<SubChannelData>,
}

/// Subscription handle detached from the event stream — see [`L4Connection::split`].
#[derive(Clone, Debug)]
pub struct L4ConnectionHandle {
    tx: UnboundedSender<SubChannelData>,
}

/// Event stream detached from subscription management — see [`L4Connection::split`].
#[derive(Debug)]
pub struct L4ConnectionStream {
    rx: UnboundedReceiver<Event>,
}

impl L4Connection {
    /// Creates a new connection. The background task starts immediately and
    /// will reconnect on failure.
    pub fn new(url: Url) -> Self {
        let (tx, rx) = unbounded_channel();
        let (stx, srx) = unbounded_channel();
        tokio::spawn(run(url, tx, srx));
        Self { rx, tx: stx }
    }

    /// Subscribes to a channel. Persists across reconnections.
    pub fn subscribe(&self, subscription: DwellirSubscription) {
        let _ = self.tx.send((true, subscription));
    }

    /// Unsubscribes from a channel.
    pub fn unsubscribe(&self, subscription: DwellirSubscription) {
        let _ = self.tx.send((false, subscription));
    }

    /// Splits the connection into an independent subscription handle and an
    /// event stream.
    pub fn split(self) -> (L4ConnectionHandle, L4ConnectionStream) {
        (
            L4ConnectionHandle { tx: self.tx },
            L4ConnectionStream { rx: self.rx },
        )
    }

    /// Closes the connection.
    pub fn close(self) {
        drop(self);
    }
}

impl futures::Stream for L4Connection {
    type Item = Event;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_recv(cx)
    }
}

impl L4ConnectionHandle {
    pub fn subscribe(&self, subscription: DwellirSubscription) {
        let _ = self.tx.send((true, subscription));
    }
    pub fn unsubscribe(&self, subscription: DwellirSubscription) {
        let _ = self.tx.send((false, subscription));
    }
    pub fn close(self) {
        drop(self);
    }
}

impl futures::Stream for L4ConnectionStream {
    type Item = Event;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_recv(cx)
    }
}

async fn run(
    url: Url,
    tx: UnboundedSender<Event>,
    mut srx: UnboundedReceiver<SubChannelData>,
) {
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    const INITIAL_BACKOFF_MS: u64 = 500;
    const MAX_BACKOFF_MS: u64 = 5_000;

    let mut subs: HashSet<DwellirSubscription> = HashSet::new();
    let mut attempts: u32 = 0;

    loop {
        let connect_result = timeout(CONNECT_TIMEOUT, Stream::connect(url.clone())).await;
        let mut stream = match connect_result {
            Ok(Ok(s)) => s,
            Ok(Err(err)) => {
                log::error!("dwellir L4: connect failed to {url}: {err:?}");
                backoff(&mut attempts, INITIAL_BACKOFF_MS, MAX_BACKOFF_MS).await;
                continue;
            }
            Err(_) => {
                log::error!("dwellir L4: connect timed out ({url})");
                backoff(&mut attempts, INITIAL_BACKOFF_MS, MAX_BACKOFF_MS).await;
                continue;
            }
        };

        attempts = 0;

        // Replay active subscriptions before advertising Connected so the
        // caller sees a ready stream.
        for sub in subs.iter() {
            if let Err(err) = stream.subscribe(sub.clone()).await {
                log::error!("dwellir L4: replay subscribe {sub} failed: {err:?}");
            }
        }
        if tx.send(Event::Connected).is_err() {
            return;
        }

        loop {
            tokio::select! {
                maybe_item = stream.next() => {
                    let Some(msg) = maybe_item else { break; };
                    if tx.send(Event::Message(msg)).is_err() {
                        return;
                    }
                }
                item = srx.recv() => {
                    let Some((is_sub, sub)) = item else { return; };
                    if is_sub {
                        if !subs.insert(sub.clone()) {
                            log::debug!("dwellir L4: already subscribed to {sub}");
                            continue;
                        }
                        if let Err(err) = stream.subscribe(sub).await {
                            log::error!("dwellir L4: subscribe failed: {err:?}");
                            break;
                        }
                    } else if subs.remove(&sub) {
                        if let Err(err) = stream.unsubscribe(sub).await {
                            log::error!("dwellir L4: unsubscribe failed: {err:?}");
                            break;
                        }
                    }
                }
            }
        }

        log::warn!("dwellir L4: disconnected from {url}, reconnecting...");
        if tx.send(Event::Disconnected).is_err() {
            return;
        }
    }
}

async fn backoff(attempts: &mut u32, initial_ms: u64, max_ms: u64) {
    let delay_ms = initial_ms.saturating_mul(1u64 << (*attempts).min(16)).min(max_ms);
    *attempts = attempts.saturating_add(1);
    log::debug!("dwellir L4: backoff {delay_ms}ms (attempt {attempts})");
    sleep(Duration::from_millis(delay_ms)).await;
}
