//! Upstream connector to Hyperliquid's realtime WebSocket (wss://api.hyperliquid.xyz/ws).
//!
//! - subscribes to trades (with on-chain wallet addresses) + 1m candles
//!   for the active coin set
//! - subscribes to `userEvents` for the largest tracked position holders,
//!   which is where liquidation events surface per user
//! - backfills candles + recent trades on coin activation
//! - reconnects with backoff, heartbeats with pings
//!
//! NOTE: only coins present in the live asset universe (or the static
//! fallback set) are ever subscribed. Hyperliquid resets the whole socket
//! when a subscription references a delisted/unknown coin, so this guard
//! is load-bearing for connection stability.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::engine::{now_ms, AppState, SubCmd};

const HL_WS_URL: &str = "wss://api.hyperliquid.xyz/ws";
const DEFAULT_COINS: usize = 16;
/// Hyperliquid rejects more than 15 `userEvents` subscriptions per
/// connection ("Cannot track more than 15 total users"), so only the very
/// largest position holders are watched.
const WATCH_USERS: usize = 15;

/// Runs the subscription managers + connection supervisor forever.
pub async fn run_upstream(state: Arc<AppState>, mut cmds: mpsc::UnboundedReceiver<SubCmd>) {
    let (coin_tx, _coin_rx) = watch::channel(HashSet::<String>::new());
    let (user_tx, _user_rx) = watch::channel(HashSet::<String>::new());

    // ---- coin subscription manager --------------------------------------------
    {
        let state = state.clone();
        let coin_tx = coin_tx.clone();
        tokio::spawn(async move {
            while let Some(cmd) = cmds.recv().await {
                match cmd {
                    SubCmd::AddCoin(c) => {
                        let mut set = coin_tx.borrow().clone();
                        if set.insert(c.clone()) {
                            tracing::info!(coin = %c, "coin activated");
                            bootstrap_coin(state.clone(), &c).await;
                            let _ = coin_tx.send(set);
                        }
                        if let Some(cs) = state.get_coin(&c) {
                            cs.inner.lock().unwrap().last_client_at = now_ms();
                        }
                    }
                    SubCmd::RemoveCoin(c) => {
                        let mut set = coin_tx.borrow().clone();
                        if set.remove(&c) {
                            tracing::info!(coin = %c, "coin deactivated");
                            let _ = coin_tx.send(set);
                        }
                    }
                }
            }
        });
    }

    // ---- whale watcher: subscribe userEvents for biggest positions -------------
    {
        let state = state.clone();
        let user_tx = user_tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let marks: std::collections::HashMap<String, f64> = state
                    .assets
                    .read()
                    .unwrap()
                    .iter()
                    .map(|a| (a.name.clone(), a.mark))
                    .collect();
                let mut notional: Vec<(String, f64)> = {
                    let traders = state.traders.read().unwrap();
                    traders
                        .iter()
                        .map(|(a, t)| {
                            let g = t.lock().unwrap();
                            let n: f64 = g
                                .positions
                                .iter()
                                .map(|(coin, p)| p.size.abs() * marks.get(coin).unwrap_or(&0.0))
                                .sum();
                            (a.clone(), n)
                        })
                        .filter(|(_, n)| *n >= 10_000.0)
                        .collect()
                };
                notional.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let set: HashSet<String> = notional
                    .into_iter()
                    .take(WATCH_USERS)
                    .map(|(a, _)| a)
                    .collect();
                if set != *user_tx.borrow() {
                    tracing::info!(n = set.len(), "watching userEvents for top holders");
                    let _ = user_tx.send(set);
                }
            }
        });
    }

    // ---- connection supervisor -------------------------------------------------
    let mut backoff = 1u64;
    loop {
        tracing::info!("connecting upstream {HL_WS_URL}");
        match tokio_tungstenite::connect_async(HL_WS_URL).await {
            Ok((ws, _resp)) => {
                backoff = 1;
                *state.upstream_alive.write().unwrap() = true;
                let _ = state
                    .broadcast
                    .send(crate::engine::EngineEvent::Status { upstream: true });
                if let Err(e) = run_connection(state.clone(), ws, coin_tx.clone(), user_tx.clone()).await {                    tracing::warn!("upstream connection ended: {e:?}");
                }
            }
            Err(e) => {
                tracing::warn!("upstream connect failed: {e}");
            }
        }
        *state.upstream_alive.write().unwrap() = false;
        let _ = state
            .broadcast
            .send(crate::engine::EngineEvent::Status { upstream: false });
        tokio::time::sleep(Duration::from_secs(backoff.min(30))).await;
        backoff = (backoff * 2).min(30);
    }
}

/// Backfill candles + recent trades when a coin is activated.
async fn bootstrap_coin(state: Arc<AppState>, coin: &str) {
    // skip if we already have fresh candles
    let (fresh_candles, stale_tape) = if let Some(cs) = state.get_coin(coin) {
        let inner = cs.inner.lock().unwrap();
        let fresh = inner
            .candles
            .values()
            .next_back()
            .map(|c| now_ms() - c.t < 5 * 60_000)
            .unwrap_or(false);
        let stale = inner
            .trades
            .back()
            .map(|t| now_ms() - t.t > 30_000)
            .unwrap_or(true);
        (fresh, stale)
    } else {
        (false, true)
    };
    if !fresh_candles {
        let start = now_ms() - 8 * 3600_000;
        match state.rest.candle_snapshot(coin, "1m", start, now_ms() + 60_000).await {
            Ok(candles) => {
                let n = candles.len();
                let parsed: Vec<shared::Candle> =
                    candles.iter().filter_map(|c| c.to_candle()).collect();
                state.seed_candles(coin, parsed);
                tracing::info!(coin, n, "bootstrapped candles");
            }
            Err(e) => tracing::warn!(coin, "candle bootstrap failed: {e}"),
        }
    }
    if stale_tape {
        match state
            .rest
            .post_json::<Vec<crate::hl::types::HlWsTrade>>(&serde_json::json!({
                "type": "recentTrades", "coin": coin
            }))
            .await
        {
            Ok(trades) => {
                let n = trades.len();
                for t in &trades {
                    state.process_trade(t);
                }
                tracing::info!(coin, n, "backfilled recent trades");
            }
            Err(e) => tracing::warn!(coin, "recentTrades backfill failed: {e}"),
        }
    }
}

type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;
type WsSink = futures_util::stream::SplitSink<WsStream, Message>;

async fn send_json(sink: &mut WsSink, v: serde_json::Value) {
    let _ = sink.send(Message::Text(v.to_string().into())).await;
}

async fn run_connection(
    state: Arc<AppState>,
    ws: WsStream,
    coin_tx: watch::Sender<HashSet<String>>,
    user_tx: watch::Sender<HashSet<String>>,
) -> anyhow::Result<()> {
    let (mut sink, mut stream) = ws.split();

    // subscribe to active coins (universe-guarded: a delisted coin would
    // otherwise get the whole socket reset by the upstream)
    let mut subscribed_coins: HashSet<String> = HashSet::new();
    let initial_coins: Vec<String> = coin_tx
        .borrow()
        .iter()
        .cloned()
        .filter(|c| state.coin_tradable(c))
        .collect();
    for c in initial_coins {
        send_json(&mut sink, serde_json::json!({"method":"subscribe","subscription":{"type":"trades","coin":c}})).await;
        send_json(&mut sink, serde_json::json!({"method":"subscribe","subscription":{"type":"candle","coin":c,"interval":"1m"}})).await;
        subscribed_coins.insert(c);
    }

    // subscribe to watched users
    let mut subscribed_users: HashSet<String> = HashSet::new();
    let initial_users: Vec<String> = user_tx.borrow().iter().cloned().collect();
    for u in initial_users {
        send_json(&mut sink, serde_json::json!({"method":"subscribe","subscription":{"type":"userEvents","user":u}})).await;
        subscribed_users.insert(u);
    }

    // NOTE: there is no market-wide liquidation channel on Hyperliquid's
    // WS API (wLiquidation/wDeleveraging are not valid subscription types
    // there); liquidations are surfaced per user through `userEvents`
    // above, plus detected behaviorally by the strict LIQ gates in the
    // engine (see engine/tuning.rs).

    // heartbeat
    let (hb_tx, mut hb_rx) = mpsc::channel::<()>(8);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(25)).await;
            if hb_tx.send(()).await.is_err() {
                break;
            }
        }
    });

    let mut coin_rx = coin_tx.subscribe();
    let mut user_rx = user_tx.subscribe();

    loop {
        let msg = tokio::select! {
            m = stream.next() => match m {
                None => anyhow::bail!("stream closed"),
                Some(Err(e)) => anyhow::bail!("ws error: {e}"),
                Some(Ok(m)) => m,
            },
            _ = hb_rx.recv() => {
                let _ = sink.send(Message::Ping(vec![1u8; 4].into())).await;
                continue;
            }
            _ = coin_rx.changed() => {
                let new_set = coin_rx.borrow_and_update().clone();
                let to_add: Vec<String> = new_set
                    .difference(&subscribed_coins)
                    .filter(|c| state.coin_tradable(c))
                    .cloned()
                    .collect();
                for c in to_add {
                    send_json(&mut sink, serde_json::json!({"method":"subscribe","subscription":{"type":"trades","coin":c}})).await;
                    send_json(&mut sink, serde_json::json!({"method":"subscribe","subscription":{"type":"candle","coin":c,"interval":"1m"}})).await;
                    subscribed_coins.insert(c.clone());
                }
                let to_rm: Vec<String> = subscribed_coins.difference(&new_set).cloned().collect();
                for c in to_rm {
                    send_json(&mut sink, serde_json::json!({"method":"unsubscribe","subscription":{"type":"trades","coin":c}})).await;
                    send_json(&mut sink, serde_json::json!({"method":"unsubscribe","subscription":{"type":"candle","coin":c,"interval":"1m"}})).await;
                    subscribed_coins.remove(&c);
                }
                continue;
            }
            _ = user_rx.changed() => {
                let new_set = user_rx.borrow_and_update().clone();
                let to_add: Vec<String> = new_set.difference(&subscribed_users).cloned().collect();
                for u in to_add {
                    send_json(&mut sink, serde_json::json!({"method":"subscribe","subscription":{"type":"userEvents","user":u}})).await;
                    subscribed_users.insert(u);
                }
                let to_rm: Vec<String> = subscribed_users.difference(&new_set).cloned().collect();
                for u in to_rm {
                    send_json(&mut sink, serde_json::json!({"method":"unsubscribe","subscription":{"type":"userEvents","user":u}})).await;
                    subscribed_users.remove(&u);
                }
                continue;
            }
        };

        let Message::Text(txt) = msg else {
            if let Message::Ping(p) = &msg {
                let _ = sink.send(Message::Pong(p.clone())).await;
            }
            continue;
        };

        let env: crate::hl::types::WsEnvelope = match serde_json::from_str(&txt) {
            Ok(e) => e,
            Err(_) => continue,
        };
        match env.channel.as_str() {
            "trades" => {
                let Ok(trades) =
                    serde_json::from_value::<Vec<crate::hl::types::HlWsTrade>>(env.data.clone())
                else {
                    continue;
                };
                for t in trades {
                    state.process_trade(&t);
                }
            }
            "candle" => {
                let Ok(arr) =
                    serde_json::from_value::<Vec<crate::hl::types::HlCandle>>(env.data.clone())
                else {
                    continue;
                };
                for c in arr {
                    if let Some(cd) = c.to_candle() {
                        state.update_candle(&c.s, cd);
                    }
                }
            }
            "userEvents" => {
                handle_user_events(&state, &env.data);
            }
            "wLiquidation" => {
                handle_user_events(&state, &env.data);
            }
            "wDeleveraging" => {
                handle_deleveraging(&state, &env.data);
            }
            "error" => {
                tracing::warn!("upstream ws error frame: {}", &txt[..txt.len().min(200)]);
            }
            _ => {}
        }
    }
}

/// Extract liquidation events from `userEvents` payloads.
/// Wire shapes handled (flexibly): {"liquidation": {"user": .., "l": [..] | {..}}}
/// as a single object or an array of events.
fn handle_user_events(state: &Arc<AppState>, v: &serde_json::Value) {
    let events: Vec<serde_json::Value> = match v {
        serde_json::Value::Array(a) => a.clone(),
        obj @ serde_json::Value::Object(_) => vec![obj.clone()],
        _ => return,
    };
    for ev in events {
        let Some(liq) = ev.get("liquidation") else { continue };
        let user = liq
            .get("user")
            .and_then(|u| u.as_str())
            .or_else(|| ev.get("user").and_then(|u| u.as_str()))
            .unwrap_or_default()
            .to_string();
        if user.is_empty() {
            continue;
        }
        let items: Vec<serde_json::Value> = match liq.get("l") {
            Some(serde_json::Value::Array(a)) => a.clone(),
            Some(obj @ serde_json::Value::Object(_)) => vec![obj.clone()],
            _ => vec![],
        };
        let t = now_ms();
        for item in items {
            let coin = item.get("coin").and_then(|x| x.as_str()).unwrap_or_default().to_string();
            if coin.is_empty() {
                continue;
            }
            let inner = crate::hl::types::HlLiqInner {
                coin,
                szi: item.get("szi").and_then(|x| x.as_str()).unwrap_or("0").to_string(),
                px: item.get("px").and_then(|x| x.as_str()).unwrap_or("0").to_string(),
                side: item.get("side").and_then(|x| x.as_str()).unwrap_or("B").to_string(),
                mark_px: None,
            };
            state.process_liquidation(&user, &inner, t);
        }
    }
}

/// Extract deleveraging events from `wDeleveraging` payloads (flexible shapes).
fn handle_deleveraging(state: &Arc<AppState>, v: &serde_json::Value) {
    let events: Vec<serde_json::Value> = match v {
        serde_json::Value::Array(a) => a.clone(),
        obj @ serde_json::Value::Object(_) => vec![obj.clone()],
        _ => return,
    };
    let t = now_ms();
    for ev in events {
        // shapes seen: {"user": .., "coin": .., "users": [..]} / {"deleveraging": {...}}
        let obj = ev.get("deleveraging").unwrap_or(&ev);
        let coin = obj
            .get("coin")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        let users: Vec<String> = obj
            .get("users")
            .and_then(|u| u.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let single = obj
            .get("user")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string());
        let mut all = users;
        if let Some(u) = single {
            all.push(u);
        }
        if !all.is_empty() && !coin.is_empty() {
            state.process_deleveraging(&all, &coin, t);
        }
    }
}

/// Background task: keep the default active coin set aligned with volume rankings.
pub async fn run_default_set(state: Arc<AppState>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(20));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let top: Vec<String> = {
            let assets = state.assets.read().unwrap();
            if assets.is_empty() {
                // REST is rate-limited / unavailable: track the majors so the
                // pipeline still fills; upgraded to the real top set later.
                crate::engine::FALLBACK_COINS
                    .iter()
                    .map(|c| c.to_string())
                    .collect()
            } else {
                assets.iter().take(DEFAULT_COINS).map(|a| a.name.clone()).collect()
            }
        };
        if top.is_empty() {
            continue;
        }
        *state.default_coins.write().unwrap() = top.clone();
        for c in top {
            let _ = state.sub_cmds.send(SubCmd::AddCoin(c.clone()));
        }
    }
}

/// Periodic gap-fill for candles of active coins.
pub async fn run_candle_gapfill(state: Arc<AppState>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(600));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let coins: Vec<Arc<crate::engine::market::CoinState>> =
            state.coins.read().unwrap().values().cloned().collect();
        for cs in coins {
            let stale = {
                let inner = cs.inner.lock().unwrap();
                inner
                    .candles
                    .values()
                    .next_back()
                    .map(|c| now_ms() - c.t > 6 * 60_000)
                    .unwrap_or(false)
            };
            if stale {
                bootstrap_coin(state.clone(), &cs.coin).await;
            }
        }
    }
}
