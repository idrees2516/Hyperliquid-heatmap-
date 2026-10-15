//! Browser-facing realtime WebSocket.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;

use crate::engine::{now_ms, AppState, SubCmd};

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| client_socket(socket, state))
}

async fn client_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut tx, mut rx) = socket.split();

    // hello
    let assets = state.assets.read().unwrap().clone();
    let hello = json!({
        "type": "hello",
        "assets": assets,
        "upstream": *state.upstream_alive.read().unwrap(),
        "server_time": now_ms(),
    });
    let _ = tx.send(Message::Text(hello.to_string().into())).await;

    let mut engine_rx = state.broadcast.subscribe();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<Option<String>>();
    let mut current_sub: Option<String> = None;

    // reader: parse client messages (subscribe / ping)
    let reader_state = state.clone();
    let reader = async move {
        while let Some(Ok(msg)) = rx.next().await {
            if let Message::Text(txt) = msg {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
                    continue;
                };
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("sub") => {
                        let coin = v
                            .get("coin")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !coin.is_empty() && coin.len() < 32 {
                            // register the coin as watched (upstream sub + refcount)
                            let cs = reader_state.get_or_create_coin(&coin);
                            {
                                let mut inner = cs.inner.lock().unwrap();
                                inner.sub_refcount += 1;
                                inner.last_client_at = now_ms();
                            }
                            let _ = reader_state.sub_cmds.send(SubCmd::AddCoin(coin.clone()));
                            let _ = sub_tx.send(Some(coin));
                        }
                    }
                    Some("unsub") => {
                        let _ = sub_tx.send(None);
                    }
                    Some("ping") => {
                        let _ = out_tx.send(Message::Text(json!({"type":"pong"}).to_string().into()));
                    }
                    _ => {}
                }
            }
        }
    };

    // writer: forward engine events (filtered) + direct messages
    let writer = async move {
        loop {
            tokio::select! {
                maybe_sub = sub_rx.recv() => {
                    match maybe_sub {
                        Some(s) => current_sub = s,
                        None => break,
                    }
                }
                out = out_rx.recv() => {
                    match out {
                        Some(m) => {
                            if tx.send(m).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                ev = engine_rx.recv() => {
                    let Ok(ev) = ev else { break };
                    let want_coin = current_sub.as_deref();
                    let txt = match &ev {
                        crate::engine::EngineEvent::Assets(a) => {
                            Some(json!({"type": "assets", "data": a}).to_string())
                        }
                        crate::engine::EngineEvent::Status { upstream } => {
                            Some(json!({"type": "status", "upstream": upstream}).to_string())
                        }
                        crate::engine::EngineEvent::Trade(t) => want_coin
                            .filter(|c| t.coin == *c)
                            .map(|_| json!({"type": "trade", "data": t}).to_string()),
                        crate::engine::EngineEvent::Liquidation(l) => {
                            Some(json!({"type": "liq", "data": l}).to_string())
                        }
                        crate::engine::EngineEvent::Candle { coin, candle } => want_coin
                            .filter(|c| *coin == *c)
                            .map(|_| json!({"type": "candle", "coin": coin, "data": candle}).to_string()),
                        crate::engine::EngineEvent::Traders { coin, rows } => want_coin
                            .filter(|c| *coin == *c)
                            .map(|_| json!({"type": "traders", "coin": coin, "data": rows}).to_string()),
                    };
                    if let Some(t) = txt {
                        if tx.send(Message::Text(t.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }
}
