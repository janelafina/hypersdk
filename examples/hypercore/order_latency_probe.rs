//! Compare order placement acknowledgement latency against user websocket updates.
//!
//! This example submits small far-from-market BTC ALO orders on mainnet, records the
//! local latency to the HTTP `/exchange` response and to matching user websocket
//! events, then cancels each resting order.

use std::{collections::HashMap, env, str::FromStr, time::Duration};

use anyhow::{Context, bail};
use clap::Parser;
use futures::StreamExt;
use hypersdk::{
    Address,
    hypercore::{
        self, BatchCancel, Cancel, Cloid, NonceHandler, OrderResponseStatus, PrivateKeySigner,
        types::{
            BatchOrder, Incoming, OrderGrouping, OrderRequest, OrderStatus, OrderTypePlacement,
            Side, Subscription, TimeInForce, UserEvent,
        },
        ws::Event,
    },
};
use rust_decimal::{Decimal, RoundingStrategy, prelude::ToPrimitive};
use tokio::{
    sync::mpsc,
    time::{Instant, sleep, timeout},
};

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    /// Number of orders to submit.
    #[arg(long, default_value_t = 100)]
    orders: usize,

    /// Milliseconds between order submissions.
    #[arg(long, default_value_t = 1000)]
    period_ms: u64,

    /// Perp symbol to trade.
    #[arg(long, default_value = "BTC")]
    coin: String,

    /// Distance below current mid for the passive bid, in basis points.
    #[arg(long, default_value_t = 3000)]
    distance_bps: u32,

    /// Minimum order notional at the passive limit price.
    #[arg(long, default_value = "15")]
    min_notional: Decimal,

    /// Time to wait for a matching open orderUpdates message before canceling.
    #[arg(long, default_value_t = 750)]
    open_update_timeout_ms: u64,

    /// Optional vault or subaccount address to pass as vaultAddress.
    #[arg(long)]
    vault_address: Option<Address>,
}

#[derive(Debug)]
enum WsMsg {
    Ack,
    Connected,
    Disconnected,
    Observation(Observation),
}

#[derive(Clone, Debug)]
struct Observation {
    cloid: Cloid,
    source: Source,
    at: Instant,
}

#[derive(Clone, Copy, Debug)]
enum Source {
    OrderUpdate { status: OrderStatus, oid: u64 },
    UserFills { oid: u64 },
    UserEventsFills { oid: u64 },
}

#[derive(Debug)]
struct Trial {
    index: usize,
    cloid: Cloid,
    oid: Option<u64>,
    sent_at: Instant,
    http_at: Option<Instant>,
    http_error: Option<String>,
    first_order_update_at: Option<Instant>,
    open_order_update_at: Option<Instant>,
    first_order_update_status: Option<OrderStatus>,
    first_fill_at: Option<Instant>,
    first_user_event_fill_at: Option<Instant>,
    cancel_ok: bool,
    cancel_error: Option<String>,
}

impl Trial {
    fn new(index: usize, cloid: Cloid, sent_at: Instant) -> Self {
        Self {
            index,
            cloid,
            oid: None,
            sent_at,
            http_at: None,
            http_error: None,
            first_order_update_at: None,
            open_order_update_at: None,
            first_order_update_status: None,
            first_fill_at: None,
            first_user_event_fill_at: None,
            cancel_ok: false,
            cancel_error: None,
        }
    }

    fn record(&mut self, obs: Observation) {
        match obs.source {
            Source::OrderUpdate { status, oid } => {
                self.oid.get_or_insert(oid);
                if self.first_order_update_at.is_none() {
                    self.first_order_update_at = Some(obs.at);
                    self.first_order_update_status = Some(status);
                }
                if matches!(status, OrderStatus::Open) && self.open_order_update_at.is_none() {
                    self.open_order_update_at = Some(obs.at);
                }
            }
            Source::UserFills { oid } => {
                self.oid.get_or_insert(oid);
                self.first_fill_at.get_or_insert(obs.at);
            }
            Source::UserEventsFills { oid } => {
                self.oid.get_or_insert(oid);
                self.first_user_event_fill_at.get_or_insert(obs.at);
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = simple_logger::init_with_level(log::Level::Info);
    dotenvy::dotenv().ok();

    let args = Cli::parse();
    if args.orders == 0 {
        bail!("--orders must be greater than zero");
    }
    if args.distance_bps >= 10_000 {
        bail!("--distance-bps must be less than 10000");
    }

    let user: Address = env::var("HYPERLIQUID_LIVE_ACCOUNT_ADDRESS")
        .context("HYPERLIQUID_LIVE_ACCOUNT_ADDRESS is required")?
        .parse()
        .context("invalid HYPERLIQUID_LIVE_ACCOUNT_ADDRESS")?;
    let signer = PrivateKeySigner::from_str(
        &env::var("HYPERLIQUID_LIVE_PRIVATE_KEY")
            .context("HYPERLIQUID_LIVE_PRIVATE_KEY is required")?,
    )
    .context("invalid HYPERLIQUID_LIVE_PRIVATE_KEY")?;

    let client = hypercore::mainnet();
    let perps = client.perps().await?;
    let market = perps
        .iter()
        .find(|perp| perp.name == args.coin)
        .with_context(|| format!("{} perp market not found", args.coin))?;
    let mids = client.all_mids(None).await?;
    let mid = *mids
        .get(&args.coin)
        .with_context(|| format!("{} mid not found", args.coin))?;
    let distance = Decimal::ONE - Decimal::from(args.distance_bps) / Decimal::from(10_000_u32);
    let raw_price = mid * distance;
    let limit_px = market
        .round_by_side(Side::Bid, raw_price, true)
        .context("failed to round passive bid price")?;
    let order_size = rounded_up_size(args.min_notional / limit_px, market.sz_decimals)?;
    if order_size * limit_px < args.min_notional {
        bail!("rounded order size is below configured min notional");
    }

    println!(
        "user={user:?} signer={:?} coin={} mid={} bid={} size={} notional={} orders={} period={}ms",
        signer.address(),
        args.coin,
        mid,
        limit_px,
        order_size,
        order_size * limit_px,
        args.orders,
        args.period_ms
    );

    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel();
    let ws = hypercore::mainnet_ws();
    ws.subscribe(Subscription::OrderUpdates { user });
    ws.subscribe(Subscription::UserFills { user });
    ws.subscribe(Subscription::UserEvents { user });
    tokio::spawn(collect_ws(ws, ws_tx));

    wait_for_subscriptions(&mut ws_rx).await?;

    let nonce = NonceHandler::default();
    let mut trials: HashMap<Cloid, Trial> = HashMap::with_capacity(args.orders);
    let mut ordered_cloids = Vec::with_capacity(args.orders);
    let period = Duration::from_millis(args.period_ms);
    let open_update_timeout = Duration::from_millis(args.open_update_timeout_ms);
    let vault_address = args.vault_address;

    for index in 0..args.orders {
        let slot_started_at = Instant::now();
        drain_ws(&mut ws_rx, &mut trials);

        let cloid = Cloid::random();
        ordered_cloids.push(cloid);
        let sent_at = Instant::now();
        trials.insert(cloid, Trial::new(index + 1, cloid, sent_at));

        let batch = BatchOrder {
            orders: vec![OrderRequest {
                asset: market.index,
                is_buy: true,
                limit_px,
                sz: order_size,
                reduce_only: false,
                order_type: OrderTypePlacement::Limit {
                    tif: TimeInForce::Alo,
                },
                cloid,
            }],
            grouping: OrderGrouping::Na,
            builder: None,
        };

        let place_result = client
            .place(&signer, batch, nonce.next(), vault_address, None)
            .await;
        let http_at = Instant::now();

        {
            let trial = trials.get_mut(&cloid).expect("trial exists");
            trial.http_at = Some(http_at);
            match place_result {
                Ok(statuses) => match statuses.first() {
                    Some(OrderResponseStatus::Resting { oid, .. }) => {
                        trial.oid = Some(*oid);
                    }
                    Some(OrderResponseStatus::Filled { oid, .. }) => {
                        trial.oid = Some(*oid);
                        trial.http_error = Some("unexpected fill".to_string());
                    }
                    Some(OrderResponseStatus::Error(err)) => {
                        trial.http_error = Some(err.clone());
                    }
                    Some(status) => {
                        trial.http_error = Some(format!("unexpected status: {status:?}"));
                    }
                    None => {
                        trial.http_error = Some("empty order response".to_string());
                    }
                },
                Err(err) => {
                    trial.http_error = Some(err.to_string());
                }
            }
        }

        wait_for_open_update(&mut ws_rx, &mut trials, cloid, open_update_timeout).await;

        if let Some(oid) = trials.get(&cloid).and_then(|trial| trial.oid) {
            let cancel = client
                .cancel(
                    &signer,
                    BatchCancel {
                        cancels: vec![Cancel {
                            asset: market.index,
                            oid,
                        }],
                    },
                    nonce.next(),
                    vault_address,
                    None,
                )
                .await;
            let trial = trials.get_mut(&cloid).expect("trial exists");
            match cancel {
                Ok(statuses) if matches!(statuses.first(), Some(OrderResponseStatus::Success)) => {
                    trial.cancel_ok = true;
                }
                Ok(statuses) => {
                    trial.cancel_error = Some(format!("{statuses:?}"));
                }
                Err(err) => {
                    trial.cancel_error = Some(err.to_string());
                }
            }
        }

        drain_ws(&mut ws_rx, &mut trials);
        print_progress(
            index + 1,
            args.orders,
            trials.get(&cloid).expect("trial exists"),
        );

        let elapsed = slot_started_at.elapsed();
        if elapsed < period {
            sleep(period - elapsed).await;
        }
    }

    drain_for(&mut ws_rx, &mut trials, Duration::from_secs(2)).await;

    retry_uncanceled(
        &client,
        &signer,
        &nonce,
        vault_address,
        market.index,
        &ordered_cloids,
        &mut trials,
    )
    .await;

    print_summary(&ordered_cloids, &trials);
    Ok(())
}

async fn collect_ws(ws: hypercore::WebSocket, tx: mpsc::UnboundedSender<WsMsg>) {
    let mut stream = Box::pin(ws);
    while let Some(event) = stream.next().await {
        match event {
            Event::Connected => {
                let _ = tx.send(WsMsg::Connected);
            }
            Event::Disconnected => {
                let _ = tx.send(WsMsg::Disconnected);
            }
            Event::Message(Incoming::SubscriptionResponse(_)) => {
                let _ = tx.send(WsMsg::Ack);
            }
            Event::Message(Incoming::OrderUpdates(updates)) => {
                for update in updates {
                    if let Some(cloid) = update.order.cloid {
                        let _ = tx.send(WsMsg::Observation(Observation {
                            cloid,
                            source: Source::OrderUpdate {
                                status: update.status,
                                oid: update.order.oid,
                            },
                            at: Instant::now(),
                        }));
                    }
                }
            }
            Event::Message(Incoming::UserFills {
                is_snapshot, fills, ..
            }) => {
                if is_snapshot {
                    continue;
                }
                for fill in fills {
                    if let Some(cloid) = fill.cloid {
                        let _ = tx.send(WsMsg::Observation(Observation {
                            cloid,
                            source: Source::UserFills { oid: fill.oid },
                            at: Instant::now(),
                        }));
                    }
                }
            }
            Event::Message(Incoming::UserEvents(UserEvent::Fills { fills })) => {
                for fill in fills {
                    if let Some(cloid) = fill.cloid {
                        let _ = tx.send(WsMsg::Observation(Observation {
                            cloid,
                            source: Source::UserEventsFills { oid: fill.oid },
                            at: Instant::now(),
                        }));
                    }
                }
            }
            _ => {}
        }
    }
}

async fn wait_for_subscriptions(rx: &mut mpsc::UnboundedReceiver<WsMsg>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut connected = false;
    let mut acks = 0_u8;
    while Instant::now() < deadline && (!connected || acks < 3) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, rx.recv()).await {
            Ok(Some(WsMsg::Connected)) => connected = true,
            Ok(Some(WsMsg::Ack)) => acks = acks.saturating_add(1),
            Ok(Some(WsMsg::Disconnected)) => {}
            Ok(Some(WsMsg::Observation(_))) => {}
            Ok(None) => bail!("websocket event stream closed before subscriptions were ready"),
            Err(_) => break,
        }
    }
    if !connected {
        bail!("websocket did not connect within 10s");
    }
    if acks < 3 {
        bail!("only received {acks}/3 subscription acknowledgements within 10s");
    }
    Ok(())
}

async fn wait_for_open_update(
    rx: &mut mpsc::UnboundedReceiver<WsMsg>,
    trials: &mut HashMap<Cloid, Trial>,
    cloid: Cloid,
    max_wait: Duration,
) {
    let deadline = Instant::now() + max_wait;
    loop {
        drain_ws(rx, trials);
        if trials
            .get(&cloid)
            .and_then(|trial| trial.open_order_update_at)
            .is_some()
        {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(25));
        match timeout(remaining, rx.recv()).await {
            Ok(Some(msg)) => apply_ws_msg(msg, trials),
            Ok(None) | Err(_) => {}
        }
    }
}

async fn drain_for(
    rx: &mut mpsc::UnboundedReceiver<WsMsg>,
    trials: &mut HashMap<Cloid, Trial>,
    duration: Duration,
) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        drain_ws(rx, trials);
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        match timeout(remaining, rx.recv()).await {
            Ok(Some(msg)) => apply_ws_msg(msg, trials),
            Ok(None) | Err(_) => {}
        }
    }
    drain_ws(rx, trials);
}

fn drain_ws(rx: &mut mpsc::UnboundedReceiver<WsMsg>, trials: &mut HashMap<Cloid, Trial>) {
    while let Ok(msg) = rx.try_recv() {
        apply_ws_msg(msg, trials);
    }
}

fn apply_ws_msg(msg: WsMsg, trials: &mut HashMap<Cloid, Trial>) {
    if let WsMsg::Observation(obs) = msg {
        if let Some(trial) = trials.get_mut(&obs.cloid) {
            trial.record(obs);
        }
    }
}

async fn retry_uncanceled(
    client: &hypercore::HttpClient,
    signer: &PrivateKeySigner,
    nonce: &NonceHandler,
    vault_address: Option<Address>,
    asset: usize,
    ordered_cloids: &[Cloid],
    trials: &mut HashMap<Cloid, Trial>,
) {
    for cloid in ordered_cloids {
        let Some(trial) = trials.get_mut(cloid) else {
            continue;
        };
        if trial.cancel_ok || trial.http_error.is_some() {
            continue;
        }
        let Some(oid) = trial.oid else {
            continue;
        };
        match client
            .cancel(
                signer,
                BatchCancel {
                    cancels: vec![Cancel { asset, oid }],
                },
                nonce.next(),
                vault_address,
                None,
            )
            .await
        {
            Ok(statuses) if matches!(statuses.first(), Some(OrderResponseStatus::Success)) => {
                trial.cancel_ok = true;
                trial.cancel_error = None;
            }
            Ok(statuses) => {
                trial.cancel_error = Some(format!("final cancel retry: {statuses:?}"));
            }
            Err(err) => {
                trial.cancel_error = Some(format!("final cancel retry: {err}"));
            }
        }
    }
}

fn rounded_up_size(size: Decimal, sz_decimals: i64) -> anyhow::Result<Decimal> {
    let dp = u32::try_from(sz_decimals).context("negative size decimals")?;
    Ok(size.round_dp_with_strategy(dp, RoundingStrategy::AwayFromZero))
}

fn print_progress(done: usize, total: usize, trial: &Trial) {
    let http_ms = trial
        .http_at
        .map(|at| elapsed_ms(trial.sent_at, at))
        .map(format_ms)
        .unwrap_or_else(|| "n/a".to_string());
    let ws_ms = trial
        .open_order_update_at
        .or(trial.first_order_update_at)
        .map(|at| elapsed_ms(trial.sent_at, at))
        .map(format_ms)
        .unwrap_or_else(|| "n/a".to_string());
    println!(
        "{done:>3}/{total}: http={http_ms} ws_order_update={ws_ms} status={:?} oid={:?} cancel_ok={}{}",
        trial.first_order_update_status,
        trial.oid,
        trial.cancel_ok,
        trial
            .http_error
            .as_ref()
            .map(|err| format!(" http_error={err}"))
            .unwrap_or_default()
    );
}

fn print_summary(ordered_cloids: &[Cloid], trials: &HashMap<Cloid, Trial>) {
    let ordered_trials: Vec<_> = ordered_cloids
        .iter()
        .filter_map(|cloid| trials.get(cloid))
        .collect();
    let mut http = Vec::new();
    let mut ws_open = Vec::new();
    let mut ws_any_order = Vec::new();
    let mut fills = Vec::new();
    let mut event_fills = Vec::new();
    let mut http_faster = 0;
    let mut ws_faster = 0;
    let mut ties = 0;

    for trial in &ordered_trials {
        if let Some(at) = trial.http_at {
            http.push(elapsed_ms(trial.sent_at, at));
        }
        if let Some(at) = trial.open_order_update_at {
            ws_open.push(elapsed_ms(trial.sent_at, at));
        }
        if let Some(at) = trial.first_order_update_at {
            ws_any_order.push(elapsed_ms(trial.sent_at, at));
        }
        if let Some(at) = trial.first_fill_at {
            fills.push(elapsed_ms(trial.sent_at, at));
        }
        if let Some(at) = trial.first_user_event_fill_at {
            event_fills.push(elapsed_ms(trial.sent_at, at));
        }
        if let (Some(http_at), Some(ws_at)) = (trial.http_at, trial.open_order_update_at) {
            match http_at.cmp(&ws_at) {
                std::cmp::Ordering::Less => http_faster += 1,
                std::cmp::Ordering::Greater => ws_faster += 1,
                std::cmp::Ordering::Equal => ties += 1,
            }
        }
    }

    let placed = ordered_trials
        .iter()
        .filter(|trial| trial.http_error.is_none() && trial.oid.is_some())
        .count();
    let http_errors = ordered_trials
        .iter()
        .filter(|trial| trial.http_error.is_some())
        .count();
    let cancel_failures: Vec<_> = ordered_trials
        .iter()
        .filter(|trial| trial.oid.is_some() && !trial.cancel_ok)
        .collect();

    println!();
    println!("summary");
    println!(
        "attempted={} placed={} http_errors={}",
        ordered_trials.len(),
        placed,
        http_errors
    );
    print_stats("http place response", &mut http);
    print_stats("ws orderUpdates open", &mut ws_open);
    print_stats("ws orderUpdates first-any-status", &mut ws_any_order);
    print_stats("ws userFills", &mut fills);
    print_stats("ws userEvents.fills", &mut event_fills);
    println!(
        "paired http-vs-open-orderUpdates: http_faster={http_faster} ws_faster={ws_faster} ties={ties}"
    );
    println!(
        "cancel_failures={}{}",
        cancel_failures.len(),
        cancel_failures
            .first()
            .and_then(|trial| trial.cancel_error.as_deref())
            .map(|err| format!(" first={err}"))
            .unwrap_or_default()
    );

    for trial in ordered_trials
        .iter()
        .filter(|trial| trial.http_error.is_some())
        .take(5)
    {
        println!(
            "http error sample order={} cloid={:?}: {}",
            trial.index,
            trial.cloid,
            trial.http_error.as_deref().unwrap_or("unknown")
        );
    }
}

fn print_stats(label: &str, values: &mut [f64]) {
    if values.is_empty() {
        println!("{label}: n=0");
        return;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let sum: f64 = values.iter().sum();
    let mean = sum / values.len() as f64;
    let min = values[0];
    let median = percentile(values, 0.50);
    let p95 = percentile(values, 0.95);
    let max = values[values.len() - 1];
    println!(
        "{label}: n={} mean={} median={} p95={} min={} max={}",
        values.len(),
        format_ms_f64(mean),
        format_ms_f64(median),
        format_ms_f64(p95),
        format_ms_f64(min),
        format_ms_f64(max)
    );
}

fn percentile(values: &[f64], p: f64) -> f64 {
    if values.len() == 1 {
        return values[0];
    }
    let rank = p * (values.len() - 1) as f64;
    let lo = rank.floor().to_usize().unwrap_or(0);
    let hi = rank.ceil().to_usize().unwrap_or(lo);
    if lo == hi {
        values[lo]
    } else {
        let frac = rank - lo as f64;
        values[lo] + (values[hi] - values[lo]) * frac
    }
}

fn elapsed_ms(start: Instant, end: Instant) -> f64 {
    end.duration_since(start).as_secs_f64() * 1000.0
}

fn format_ms(ms: f64) -> String {
    format_ms_f64(ms)
}

fn format_ms_f64(ms: f64) -> String {
    format!("{ms:.2}ms")
}
