//! REST API handlers.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::engine::{now_ms, AppState};

pub struct ApiError(pub String);

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, self.0).into_response()
    }
}

pub async fn assets(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let assets = state.assets.read().unwrap().clone();
    Json(json!({ "assets": assets, "upstream": *state.upstream_alive.read().unwrap() }))
}

#[derive(serde::Deserialize)]
pub struct FootQuery {
    pub coin: String,
    /// interval in milliseconds
    #[serde(default = "default_interval")]
    pub interval: i64,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_bucket_mult")]
    pub bucket_mult: f64,
}

fn default_interval() -> i64 {
    60_000
}
fn default_limit() -> usize {
    120
}
fn default_bucket_mult() -> f64 {
    1.0
}

const ALLOWED_INTERVALS: [i64; 5] = [15_000, 60_000, 300_000, 900_000, 3_600_000];

pub async fn footprint(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FootQuery>,
) -> Result<Json<shared::FootprintSnapshot>, ApiError> {
    let interval = if ALLOWED_INTERVALS.contains(&q.interval) { q.interval } else { 60_000 };
    let state = &state;
    let coin = state.get_or_create_coin(&q.coin);
    let inner = coin.inner.lock().unwrap();
    let mut snapshot = crate::engine::market::build_footprint(
        &q.coin,
        &inner,
        interval,
        q.limit,
        q.bucket_mult,
        now_ms(),
        &|addr| state.trader_snapshot(addr),
    );
    // ensure at least the current (possibly empty) window exists
    if snapshot.windows.is_empty() {
        // fall back to candles-only view
        let cur = shared::window_start(now_ms(), interval);
        snapshot.windows.push(shared::FootWindow {
            t: cur,
            o: 0.0,
            h: 0.0,
            l: 0.0,
            c: 0.0,
            buy: 0.0,
            sell: 0.0,
            delta: 0.0,
            poc: 0.0,
            wallets: 0,
            prints: 0,
            levels: vec![],
            tb: vec![],
            ts: vec![],
        });
    }
    Ok(Json(snapshot))
}

#[derive(serde::Deserialize)]
pub struct LevelQuery {
    pub coin: String,
    /// window start, ms
    pub t: i64,
    /// interval, ms
    #[serde(default = "default_interval")]
    pub interval: i64,
    /// bucket lower bound price
    pub p_lo: f64,
    #[serde(default = "default_bucket_mult")]
    pub bucket_mult: f64,
}

pub async fn level(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LevelQuery>,
) -> Result<Json<shared::LevelDetail>, ApiError> {
    let interval = if ALLOWED_INTERVALS.contains(&q.interval) { q.interval } else { 60_000 };
    let state_ref = &state;
    let coin = state.get_or_create_coin(&q.coin);
    let inner = coin.inner.lock().unwrap();
    let now = now_ms();
    let mark = inner.last_px;
    let bucket = shared::bucket_size(
        mark,
        q.bucket_mult,
        if inner.tick_est > 0.0 { inner.tick_est } else { 0.0001 },
    );
    let t0 = q.t;
    let t1 = (t0 + interval).min(now + interval);
    let p_hi = q.p_lo + bucket;
    let detail = crate::engine::market::build_level_detail(
        &q.coin,
        &inner,
        t0,
        t1,
        q.p_lo,
        p_hi,
        bucket,
        now,
        &|addr| state_ref.trader_snapshot(addr),
    );
    Ok(Json(detail))
}

#[derive(serde::Deserialize)]
pub struct TradersQuery {
    pub coin: String,
    #[serde(default = "default_minutes")]
    pub minutes: i64,
}

fn default_minutes() -> i64 {
    60
}

pub async fn traders(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TradersQuery>,
) -> impl IntoResponse {
    let rows = state.build_traders(&q.coin, q.minutes.clamp(5, 720));
    Json(json!({ "coin": q.coin, "rows": rows }))
}

pub async fn trader_detail(
    State(state): State<Arc<AppState>>,
    Path(addr): Path<String>,
) -> impl IntoResponse {
    let snap = state.trader_snapshot(&addr);
    match snap {
        Some(s) => Json(json!({
            "addr": addr,
            "style": s.style,
            "tag": s.tag,
            "pnl": s.pnl,
            "win_rate": s.win_rate,
            "account_value": s.account_value,
            "label": s.label,
            "first_seen": s.first_seen,
            "liq_count": s.liq_count,
        }))
        .into_response(),
        None => (StatusCode::NOT_FOUND, "unknown wallet").into_response(),
    }
}

pub async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let coins = state.coins.read().unwrap().len();
    let traders = state.traders.read().unwrap().len();
    Json(json!({
        "ok": true,
        "upstream": *state.upstream_alive.read().unwrap(),
        "coins_tracked": coins,
        "wallets_tracked": traders,
        "uptime_s": (now_ms() - state.started_at) / 1000,
    }))
}
