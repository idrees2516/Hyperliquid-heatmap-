//! Shared types + footprint aggregation logic.
//! Used by both the axum backend (native) and the Leptos frontend (wasm),
//! so this crate must stay wasm-safe (serde only, no I/O).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Side of the *taker* (aggressor). B = taker bought (lifted offer),
/// A = taker sold (hit bid). Matches Hyperliquid's `trades` WS channel.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    B,
    A,
}

impl Side {
    pub fn is_buy(self) -> bool {
        matches!(self, Side::B)
    }
    pub fn sign(self) -> f64 {
        if self.is_buy() { 1.0 } else { -1.0 }
    }
}

/// Inferred intent behind a taker fill.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// Plain aggressive speculation (open/increase).
    Aggressive,
    /// Liquidation cascade fill (taker was liquidated).
    Liquidation,
    /// Deleveraging event fill.
    Deleveraging,
    /// Reducing fill that realized a loss -> likely triggered stop.
    StopLoss,
    /// Reducing fill that realized a profit.
    TakeProfit,
    /// Cross-market delta-neutral hedge leg.
    Hedge,
    /// Aggressive reduce with no pnl signal / flat-to-flat.
    RiskReduce,
    /// Unknown / default.
    Unknown,
}

impl Intent {
    pub fn label(self) -> &'static str {
        match self {
            Intent::Aggressive => "AGGR",
            Intent::Liquidation => "LIQ",
            Intent::Deleveraging => "DELEV",
            Intent::StopLoss => "STOP",
            Intent::TakeProfit => "TP",
            Intent::Hedge => "HEDGE",
            Intent::RiskReduce => "REDUCE",
            Intent::Unknown => "—",
        }
    }
    pub fn color(self) -> &'static str {
        match self {
            Intent::Aggressive => "#7dd3fc",
            Intent::Liquidation => "#f97316",
            Intent::Deleveraging => "#fb7185",
            Intent::StopLoss => "#ef4444",
            Intent::TakeProfit => "#22c55e",
            Intent::Hedge => "#a78bfa",
            Intent::RiskReduce => "#fbbf24",
            Intent::Unknown => "#94a3b8",
        }
    }
}

/// Taker position action relative to their tracked position before the fill.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Open,
    Increase,
    Reduce,
    Flip,
    Flat,
}

// ---------------------------------------------------------------------------
// Market metadata / candles
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetInfo {
    pub name: String,
    pub mark: f64,
    pub prev_day: f64,
    pub day_ntl_vlm: f64,
    pub funding: f64,
    pub open_interest: f64,
    pub sz_decimals: u32,
    pub max_leverage: u32,
}

impl AssetInfo {
    pub fn change24h(&self) -> f64 {
        if self.prev_day > 0.0 {
            self.mark / self.prev_day - 1.0
        } else {
            0.0
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Candle {
    pub t: i64, // window start, ms
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64, // base volume
    pub n: i64, // trade count
}

impl Default for Candle {
    fn default() -> Self {
        Candle { t: 0, o: 0.0, h: 0.0, l: 0.0, c: 0.0, v: 0.0, n: 0 }
    }
}

impl Candle {
    /// Merge a sub-candle into self (self.t must be the earliest start).
    pub fn merge(&mut self, sub: &Candle) {
        self.h = self.h.max(sub.h);
        self.l = if self.l == 0.0 { sub.l } else { self.l.min(sub.l) };
        self.c = sub.c;
        self.v += sub.v;
        self.n += sub.n;
    }
}

// ---------------------------------------------------------------------------
// Footprint
// ---------------------------------------------------------------------------

/// One price-level aggregate inside a time window (all USD notional).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LevelAgg {
    /// bucket center price
    pub p: f64,
    /// taker buy volume (USD)
    pub buy: f64,
    /// taker sell volume (USD)
    pub sell: f64,
    /// prints (fills)
    pub n: u32,
    /// liquidation-driven volume
    pub liq: f64,
    /// stop-loss driven volume
    pub stop: f64,
    /// take-profit driven volume
    pub tp: f64,
    /// hedge volume
    pub hedge: f64,
}

impl LevelAgg {
    pub fn total(&self) -> f64 {
        self.buy + self.sell
    }
    pub fn delta(&self) -> f64 {
        self.buy - self.sell
    }
}

/// Top-wallet rollup inside a window (side-specific).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowTrader {
    pub a: String,
    pub b: f64, // bought usd
    pub s: f64, // sold usd
    pub tag: String,
    pub pnl: Option<f64>,
    pub liq: bool,
}

/// One footprint window: a candle + per-price-level aggregates.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FootWindow {
    pub t: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub buy: f64,
    pub sell: f64,
    pub delta: f64,
    pub poc: f64,
    pub wallets: u32,
    pub prints: u32,
    pub levels: Vec<LevelAgg>,
    pub tb: Vec<WindowTrader>, // top buyers
    pub ts: Vec<WindowTrader>, // top sellers
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FootprintSnapshot {
    pub coin: String,
    pub interval_ms: i64,
    pub bucket: f64,
    pub bucket_mult: f64,
    pub tick: f64,
    pub candles: Vec<Candle>,
    pub windows: Vec<FootWindow>,
    pub generated_at: i64,
}

// ---------------------------------------------------------------------------
// Level detail (drill-down modal)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ActionSplit {
    pub open: f64,
    pub increase: f64,
    pub reduce: f64,
    pub flip: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalletRow {
    pub a: String,
    pub label: Option<String>,
    pub style: String,
    /// conviction tag: "++", "+", "±", "-", "--"
    pub tag: String,
    pub bought: f64,
    pub sold: f64,
    pub net: f64,
    pub share: f64,
    pub rpn: f64,
    pub pnl_all: Option<f64>,
    pub win_rate: Option<f64>,
    pub liq: bool,
    pub is_maker: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LevelDetail {
    pub coin: String,
    pub t_start: i64,
    pub t_end: i64,
    pub p_lo: f64,
    pub p_hi: f64,
    pub buy_usd: f64,
    pub sell_usd: f64,
    pub delta: f64,
    pub prints: u32,
    pub wallets: u32,
    pub aggressor_usd: f64,
    pub resting_usd: f64,
    pub actions: ActionSplit,
    pub rpn_buyers: f64,
    pub rpn_sellers: f64,
    pub liq_usd: f64,
    pub wallets_rows: Vec<WalletRow>,
}

// ---------------------------------------------------------------------------
// Traders leaderboard
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraderRow {
    pub a: String,
    pub label: Option<String>,
    pub style: String,
    pub tag: String,
    /// realized pnl over observed + enriched recent fills (USD)
    pub pnl: Option<f64>,
    pub account_value: Option<f64>,
    pub win_rate: Option<f64>,
    pub bought: f64,
    pub sold: f64,
    pub trades: u32,
    pub share: f64,
    pub maker_ratio: Option<f64>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub liq_count: u32,
}

// ---------------------------------------------------------------------------
// Live stream messages (server -> browser)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveTrade {
    pub coin: String,
    pub t: i64,
    pub px: f64,
    pub sz: f64,
    pub usd: f64,
    pub side: Side,
    pub taker: String,
    pub maker: String,
    pub intent: Intent,
    pub action: Action,
    /// realized pnl of the taker on this fill, if reducing
    pub rpn: f64,
    pub taker_style: String,
    pub taker_pnl: Option<f64>,
    pub taker_tag: String,
    pub taker_label: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveLiquidation {
    pub coin: String,
    pub user: String,
    pub sz: f64,
    pub usd: f64,
    pub px: f64,
    /// direction of the liquidated position: true = long liquidated
    pub long: bool,
    pub t: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CoinStats {
    pub coin: String,
    pub buy: f64,
    pub sell: f64,
    pub delta: f64,
    pub prints: u32,
    pub wallets: u32,
    pub liq_count: u32,
    pub liq_usd: f64,
    pub stop_usd: f64,
    pub tp_usd: f64,
    pub hedge_usd: f64,
}

// ---------------------------------------------------------------------------
// Bucket sizing
// ---------------------------------------------------------------------------

/// Round to a "nice" number: 1 / 2 / 2.5 / 5 x 10^n
pub fn nice_step(raw: f64) -> f64 {
    if raw <= 0.0 || !raw.is_finite() {
        return 0.1;
    }
    let exp = (raw.log10().floor()) as i32;
    let base = 10f64.powi(exp);
    let frac = raw / base;
    let nice = if frac <= 1.0 {
        1.0
    } else if frac <= 2.0 {
        2.0
    } else if frac <= 2.5 {
        2.5
    } else if frac <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * base
}

/// Default footprint bucket size for a coin trading around `price`.
/// ~2.5 bps of price, rounded to a nice number, scaled by user multiplier.
pub fn bucket_size(price: f64, mult: f64, tick: f64) -> f64 {
    let raw = price * 0.00025 * mult.max(0.1);
    let b = nice_step(raw).max(tick);
    b
}

/// Bucket index of a price.
#[inline]
pub fn bucket_idx(price: f64, bucket: f64) -> i64 {
    (price / bucket).floor() as i64
}

/// Center price of a bucket index.
#[inline]
pub fn bucket_center(idx: i64, bucket: f64) -> f64 {
    (idx as f64 + 0.5) * bucket
}

/// Align a timestamp down to its interval window.
#[inline]
pub fn window_start(t_ms: i64, interval_ms: i64) -> i64 {
    t_ms - (t_ms.rem_euclid(interval_ms))
}

// ---------------------------------------------------------------------------
// Formatting helpers (used by frontend)
// ---------------------------------------------------------------------------

pub fn fmt_usd(v: f64) -> String {
    let av = v.abs();
    if av >= 1_000_000_000.0 {
        format!("${:.2}B", v / 1e9)
    } else if av >= 1_000_000.0 {
        format!("${:.2}M", v / 1e6)
    } else if av >= 1_000.0 {
        format!("${:.1}K", v / 1e3)
    } else {
        format!("${:.0}", v)
    }
}

pub fn fmt_qty(v: f64) -> String {
    let av = v.abs();
    if av >= 1_000_000.0 {
        format!("{:.2}M", v / 1e6)
    } else if av >= 10_000.0 {
        format!("{:.1}K", v / 1e3)
    } else if av >= 100.0 {
        format!("{:.0}", v)
    } else if av >= 1.0 {
        format!("{:.2}", v)
    } else if av >= 0.01 {
        format!("{:.4}", v)
    } else if av > 0.0 {
        format!("{:.6}", v)
    } else {
        "0".to_string()
    }
}

/// Conviction tag from trader pnl percentile (-1..1)
pub fn conviction_tag(score: f64) -> &'static str {
    if score >= 0.75 {
        "++"
    } else if score >= 0.25 {
        "+"
    } else if score > -0.25 {
        "±"
    } else if score > -0.75 {
        "-"
    } else {
        "--"
    }
}

/// Short address 0x1234…abcd
pub fn short_addr(a: &str) -> String {
    if a.len() >= 10 {
        format!("{}…{}", &a[..6], &a[a.len() - 4..])
    } else {
        a.to_string()
    }
}
