//! Bookmap terminal state: sampled L2 book history per coin.
//!
//! Hyperliquid's `l2Book` WS channel pushes a *full* snapshot of the
//! aggregated book on every change. We
//!
//! - keep the latest snapshot as the live book (DOM ladder / depth chart),
//!   rebroadcast to browsers at a throttled rate, and
//! - sample a column every `SAMPLE_MS` into a ring buffer, which is the
//!     heatmap history (bookmap-style resting-liquidity map).
//!
//! Only coins a browser client is actively watching are subscribed upstream
//! (see `SubCmd::WatchBook`), because full-book snapshots are chatty.

use std::collections::VecDeque;

use shared::{BookCol, BookLevel, BookPayload, LiveLiquidation, LiveTrade};

use crate::engine::{now_ms, AppState};

/// heatmap column sampling cadence
pub const SAMPLE_MS: i64 = 400;
/// retained history (~48 min at 400 ms)
pub const MAX_COLS: usize = 7_200;
/// levels stored per side in a history column
pub const COL_LEVELS: usize = 110;
/// levels per side in the compact live-book broadcast
pub const LIVE_LEVELS: usize = 40;
/// min spacing between live-book broadcasts (the DOM ladder still feels
/// continuous; the heatmap columns carry the fine structure)
pub const LIVE_PUSH_MS: i64 = 120;

pub struct BookState {
    pub coin: String,
    pub cols: VecDeque<BookCol>,
    /// latest compacted snapshot (DOM ladder / depth source)
    pub cur: Option<BookCol>,
    last_sample: i64,
    last_live_push: i64,
}

impl BookState {
    pub fn new(coin: &str) -> Self {
        BookState {
            coin: coin.to_string(),
            cols: VecDeque::new(),
            cur: None,
            last_sample: 0,
            last_live_push: 0,
        }
    }

    /// Feed one upstream snapshot. Returns `(sampled_column, push_live)`.
    pub fn on_snapshot(
        &mut self,
        t: i64,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
    ) -> (Option<BookCol>, bool) {
        if bids.is_empty() || asks.is_empty() {
            return (None, false);
        }
        let cur = BookCol {
            t,
            bids: truncate(bids, COL_LEVELS),
            asks: truncate(asks, COL_LEVELS),
        };
        let push_live = t - self.last_live_push >= LIVE_PUSH_MS;
        if push_live {
            self.last_live_push = t;
        }
        self.cur = Some(cur.clone());

        let mut sampled = None;
        if t - self.last_sample >= SAMPLE_MS {
            self.last_sample = t;
            let col = BookCol {
                t,
                bids: truncate(cur.bids.clone(), COL_LEVELS),
                asks: truncate(cur.asks.clone(), COL_LEVELS),
            };
            self.cols.push_back(col.clone());
            while self.cols.len() > MAX_COLS {
                self.cols.pop_front();
            }
            sampled = Some(col);
        }
        (sampled, push_live)
    }

    /// Compact broadcast version of the current book.
    pub fn live_book(&self) -> Option<BookCol> {
        self.cur.as_ref().map(|c| BookCol {
            t: c.t,
            bids: truncate(c.bids.clone(), LIVE_LEVELS),
            asks: truncate(c.asks.clone(), LIVE_LEVELS),
        })
    }

    /// Build a REST payload with at most `max_cols` columns (downsampled
    /// evenly so the visible span is preserved).
    pub fn payload(&self, coin: &str, max_cols: usize, trades: Vec<LiveTrade>, liqs: Vec<LiveLiquidation>) -> BookPayload {
        let n = self.cols.len();
        let cols: Vec<BookCol> = if n == 0 || max_cols >= n {
            self.cols.iter().cloned().collect()
        } else if max_cols <= 1 {
            self.cols.back().cloned().into_iter().collect()
        } else {
            let step = n as f64 / max_cols as f64;
            let mut out = Vec::with_capacity(max_cols);
            let mut i = 0usize;
            while i < max_cols {
                let idx = ((i as f64) * step).floor() as usize;
                let idx = idx.min(n - 1);
                out.push(self.cols[idx].clone());
                i += 1;
            }
            // always include the newest column
            let last = self.cols.back().unwrap().clone();
            if out.last().map(|c| c.t) != Some(last.t) {
                out.pop();
                out.push(last);
            }
            out
        };
        BookPayload {
            coin: coin.to_string(),
            cols,
            book: self.live_book(),
            trades,
            liqs,
            sample_ms: SAMPLE_MS,
            hist_cols: n,
        }
    }
}

fn truncate(mut v: Vec<BookLevel>, n: usize) -> Vec<BookLevel> {
    if v.len() > n {
        v.truncate(n);
    }
    v
}

/// Parse raw wire levels (px/sz strings, n number-or-string) into typed
/// levels, sorted best-first, invalid rows dropped.
pub fn parse_levels(raw: &[crate::hl::types::HlBookLevel], bids: bool) -> Vec<BookLevel> {
    let mut out: Vec<BookLevel> = raw
        .iter()
        .filter_map(|l| {
            let px: f64 = l.px.parse().ok()?;
            let sz: f64 = l.sz.parse().ok()?;
            if !px.is_finite() || !sz.is_finite() || px <= 0.0 || sz <= 0.0 {
                return None;
            }
            let n = match &l.n {
                serde_json::Value::Number(x) => x.as_f64().unwrap_or(0.0),
                serde_json::Value::String(s) => s.parse().unwrap_or(0.0),
                _ => 0.0,
            };
            Some(BookLevel { px, sz, n })
        })
        .collect();
    if bids {
        out.sort_by(|a, b| b.px.partial_cmp(&a.px).unwrap_or(std::cmp::Ordering::Equal));
    } else {
        out.sort_by(|a, b| a.px.partial_cmp(&b.px).unwrap_or(std::cmp::Ordering::Equal));
    }
    out
}

// ---------------------------------------------------------------------------
// REST book poller
// ---------------------------------------------------------------------------

/// The public `l2Book` WS channel only pushes a snapshot every few seconds
/// (throttled upstream), so the heatmap is densified by polling the REST
/// `l2Book` endpoint for coins a browser client is actively watching.
/// Rate-limit budget: l2Book costs weight 2; two coins at ~2 Hz stays far
/// below Hyperliquid's 1200 weight/min cap.
const POLL_MIN_MS: i64 = 450;
const POLL_MAX_COINS: usize = 2;

pub async fn run_book_poll(state: std::sync::Arc<AppState>) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(220));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_poll: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    loop {
        ticker.tick().await;
        let now = now_ms();
        // watched coins, least-recently-polled first, budgeted
        let mut coins: Vec<String> = state
            .book_watch
            .read()
            .unwrap()
            .iter()
            .filter(|(_, n)| **n > 0)
            .map(|(c, _)| c.clone())
            .collect();
        coins.sort_by_key(|c| last_poll.get(c).copied().unwrap_or(0));
        for coin in coins.into_iter().take(POLL_MAX_COINS) {
            if now - last_poll.get(&coin).copied().unwrap_or(0) < POLL_MIN_MS {
                continue;
            }
            last_poll.insert(coin.clone(), now);
            match state
                .rest
                .post_json::<crate::hl::types::HlBook>(&serde_json::json!({
                    "type": "l2Book", "coin": coin
                }))
                .await
            {
                Ok(b) => state.process_book(&b),
                Err(e) => {
                    tracing::debug!(coin = %coin, "l2Book poll failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
            }
        }
    }
}
