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
// Tuning (live-tunable engine parameters)
// ---------------------------------------------------------------------------

/// Live-tunable engine parameters (see the TUNING tab).
/// All thresholds that gate intent classification + radar/opportunity engines.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TuningParams {
    // --- liquidation intent precision -----------------------------------
    /// minimum confidence (0..1) for a behavioral LIQ tag. higher = stricter.
    pub liq_conf_cap: f64,
    /// minimum sweep slippage vs the 30s-ago reference price, in bps, for a
    /// behavioral LIQ tag. higher = stricter (only deep book-sweeps count).
    pub liq_min_sweep_slip_bps: f64,
    /// minimum position notional (USD) for a behavioral LIQ tag.
    pub liq_min_usd: f64,
    /// minimum fraction of the position closed by the burst (0..1).
    pub liq_min_close_frac: f64,
    /// minimum realized loss depth (bps of notional) for a behavioral LIQ tag.
    pub liq_min_loss_bps: f64,

    // --- radar / magnet levels ------------------------------------------
    /// radar bucket width as bps of price.
    pub radar_bucket_bps: f64,
    /// minimum estimated forced-flow (USD) for a level to count as a magnet.
    pub radar_level_usd: f64,
    /// level half-life (minutes): armed estimates decay as positions adjust.
    pub radar_decay_min: f64,
    /// a crossed level re-arms (est halved) if no forced flow confirms it
    /// within this window (ms).
    pub radar_confirm_ms: i64,

    // --- cascades ---------------------------------------------------------
    /// chained-burst gap that finalizes a cascade record (ms).
    pub cascade_burst_ms: i64,
    /// maximum cascade duration before forced finalize (ms).
    pub cascade_max_ms: i64,
    /// minimum total forced USD for a burst to be recorded as a cascade.
    pub cascade_min_usd: f64,

    // --- opportunities -----------------------------------------------------
    /// minimum est overhang USD for a magnet-pull opportunity.
    pub opp_magnet_usd: f64,
    /// max distance (%) from price for a magnet to be actionable.
    pub opp_magnet_dist_pct: f64,
    /// minimum stop-cluster USD for a stop-run opportunity.
    pub opp_stop_usd: f64,
    /// minimum whale print USD for a follow-smart-money opportunity.
    pub opp_whale_usd: f64,
}

impl Default for TuningParams {
    fn default() -> Self {
        TuningParams {
            // strict liquidation intent: high confidence + real sweep depth
            liq_conf_cap: 0.55,
            liq_min_sweep_slip_bps: 3.0,
            liq_min_usd: 5_000.0,
            liq_min_close_frac: 0.85,
            liq_min_loss_bps: 8.0,

            radar_bucket_bps: 25.0,
            radar_level_usd: 40_000.0,
            radar_decay_min: 240.0,
            radar_confirm_ms: 20_000,

            cascade_burst_ms: 6_000,
            cascade_max_ms: 180_000,
            cascade_min_usd: 10_000.0,

            opp_magnet_usd: 150_000.0,
            opp_magnet_dist_pct: 1.5,
            opp_stop_usd: 100_000.0,
            opp_whale_usd: 250_000.0,
        }
    }
}

impl TuningParams {
    /// Clamp every field into a sane operating range.
    pub fn clamped(&self) -> Self {
        let mut p = self.clone();
        p.liq_conf_cap = p.liq_conf_cap.clamp(0.05, 0.99);
        p.liq_min_sweep_slip_bps = p.liq_min_sweep_slip_bps.clamp(0.0, 200.0);
        p.liq_min_usd = p.liq_min_usd.clamp(100.0, 5_000_000.0);
        p.liq_min_close_frac = p.liq_min_close_frac.clamp(0.30, 1.0);
        p.liq_min_loss_bps = p.liq_min_loss_bps.clamp(0.0, 2_000.0);
        p.radar_bucket_bps = p.radar_bucket_bps.clamp(5.0, 500.0);
        p.radar_level_usd = p.radar_level_usd.clamp(1_000.0, 50_000_000.0);
        p.radar_decay_min = p.radar_decay_min.clamp(10.0, 10_080.0);
        p.radar_confirm_ms = p.radar_confirm_ms.clamp(2_000, 120_000);
        p.cascade_burst_ms = p.cascade_burst_ms.clamp(1_000, 60_000);
        p.cascade_max_ms = p.cascade_max_ms.clamp(30_000, 900_000);
        p.cascade_min_usd = p.cascade_min_usd.clamp(1_000.0, 10_000_000.0);
        p.opp_magnet_usd = p.opp_magnet_usd.clamp(5_000.0, 50_000_000.0);
        p.opp_magnet_dist_pct = p.opp_magnet_dist_pct.clamp(0.1, 10.0);
        p.opp_stop_usd = p.opp_stop_usd.clamp(1_000.0, 50_000_000.0);
        p.opp_whale_usd = p.opp_whale_usd.clamp(10_000.0, 50_000_000.0);
        p
    }
}

// ---------------------------------------------------------------------------
// Radar: magnet levels / cascades / overhang
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LevelState {
    /// armed estimate: forced flow would trigger here if price reaches it
    Armed,
    /// price is trading through the level, awaiting forced-flow confirmation
    Engaged,
    /// liquidated / consumed
    Swept,
}

impl LevelState {
    pub fn label(self) -> &'static str {
        match self {
            LevelState::Armed => "ARMED",
            LevelState::Engaged => "ENGAGED",
            LevelState::Swept => "SWEPT",
        }
    }
}

/// One magnet level on the liquidation radar.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RadarLevel {
    /// bucket center price
    pub px: f64,
    /// true = long positions liquidate here (sell pressure below price),
    /// false = short positions liquidate here (buy pressure above price)
    pub long: bool,
    /// estimated forced-flow notional still armed (USD)
    pub est_usd: f64,
    /// peak estimated notional this level has carried
    pub peak_usd: f64,
    /// observed forced flow at this level (USD)
    pub swept_usd: f64,
    pub state: LevelState,
    pub t_first: i64,
    pub t_last: i64,
    /// number of times this level has been crossed
    pub hits: u32,
}

/// A completed (or live) liquidation cascade record.
/// Persisted so overhang signals survive restarts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CascadeRecord {
    pub coin: String,
    pub t_start: i64,
    pub t_end: i64,
    /// true = longs were liquidated (forced selling, downside move)
    pub long: bool,
    pub usd: f64,
    pub fills: u32,
    pub wallets: u32,
    pub px_lo: f64,
    pub px_hi: f64,
    /// deepest observed sweep slippage (bps)
    pub slip_bps: f64,
    /// armed-level estimates consumed on the way (bucket prices)
    pub levels: Vec<f64>,
    pub live: bool,
}

/// Overhang summary: armed forced-flow within reach of the current price.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OverhangInfo {
    pub px: f64,
    /// total armed long-liq flow within 5% below price
    pub long_usd_5: f64,
    /// total armed short-liq flow within 5% above price
    pub short_usd_5: f64,
    /// nearest armed long-liq level below price
    pub long_nearest_px: Option<f64>,
    pub long_nearest_usd: f64,
    /// nearest armed short-liq level above price
    pub short_nearest_px: Option<f64>,
    pub short_nearest_usd: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RadarSnapshot {
    pub coin: String,
    pub px: f64,
    pub bucket: f64,
    pub levels: Vec<RadarLevel>,
    pub overhang: OverhangInfo,
    pub last_cascade: Vec<CascadeRecord>,
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// Opportunities
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OppKind {
    /// armed magnet level near price — price tends to be pulled into it
    MagnetPull,
    /// cascade just completed — forced flow exhausted, mean-reversion window
    CascadeExhaustion,
    /// cascade printing right now
    SweepLive,
    /// stop cluster above/below price got run — continuation
    StopRun,
    /// one-sided aggressive delta with flat price = passive absorption
    DeltaDivergence,
    /// large print from a historically winning wallet
    WhalePrint,
    /// swept cascade level acting as support/resistance on retest
    SweptLevel,
}

impl OppKind {
    pub fn label(self) -> &'static str {
        match self {
            OppKind::MagnetPull => "MAGNET",
            OppKind::CascadeExhaustion => "EXHAUST",
            OppKind::SweepLive => "SWEEP",
            OppKind::StopRun => "STOP-RUN",
            OppKind::DeltaDivergence => "ABSORB",
            OppKind::WhalePrint => "WHALE",
            OppKind::SweptLevel => "S/R",
        }
    }
    pub fn color(self) -> &'static str {
        match self {
            OppKind::MagnetPull => "#f5c451",
            OppKind::CascadeExhaustion => "#97fce4",
            OppKind::SweepLive => "#f97316",
            OppKind::StopRun => "#ef4444",
            OppKind::DeltaDivergence => "#a78bfa",
            OppKind::WhalePrint => "#22c55e",
            OppKind::SweptLevel => "#7dd3fc",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Opportunity {
    pub id: String,
    pub kind: OppKind,
    pub coin: String,
    pub title: String,
    pub detail: String,
    /// 0..100, computed by the engine
    pub strength: f64,
    pub t: i64,
    pub ref_px: f64,
    pub target_px: Option<f64>,
    /// true = bullish bias, false = bearish, None = neutral/setup
    pub side_bias: Option<bool>,
    /// how long this row stays live (ms)
    pub ttl_ms: i64,
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

// ---------------------------------------------------------------------------
// Bookmap terminal (live L2 book history)
// ---------------------------------------------------------------------------

/// upper bound on returned heatmap columns (server retains more)
pub const MAX_BOOK_COLS: usize = 7_200;

/// One resting-order level in a book snapshot.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BookLevel {
    pub px: f64,
    /// base-asset size resting at the level
    pub sz: f64,
    /// number of aggregated orders at the level (Hyperliquid reports it)
    #[serde(default)]
    pub n: f64,
}

impl BookLevel {
    pub fn usd(&self) -> f64 {
        self.px * self.sz
    }
}

/// One sampled book snapshot — a single heatmap column.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BookCol {
    pub t: i64,
    /// best (highest) bid first
    pub bids: Vec<BookLevel>,
    /// best (lowest) ask first
    pub asks: Vec<BookLevel>,
}

impl BookCol {
    pub fn mid(&self) -> f64 {
        match (self.bids.first(), self.asks.first()) {
            (Some(b), Some(a)) => (b.px + a.px) / 2.0,
            (Some(b), None) => b.px,
            (None, Some(a)) => a.px,
            (None, None) => 0.0,
        }
    }
    pub fn spread_bps(&self) -> f64 {
        let (Some(b), Some(a)) = (self.bids.first(), self.asks.first()) else { return 0.0 };
        if b.px <= 0.0 { return 0.0 }
        (a.px - b.px) / b.px * 10_000.0
    }
}

/// REST payload for the bookmap terminal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BookPayload {
    pub coin: String,
    /// downsampled column history, oldest first
    pub cols: Vec<BookCol>,
    /// latest compact book snapshot (DOM ladder / depth chart source)
    pub book: Option<BookCol>,
    /// recent prints for the trade bubbles
    pub trades: Vec<LiveTrade>,
    /// recent liquidations
    pub liqs: Vec<LiveLiquidation>,
    /// server sampling cadence (ms per heatmap column)
    pub sample_ms: i64,
    /// total columns retained server-side
    pub hist_cols: usize,
}
