//! Trader registry: per-wallet position tracking, realized-PnL accounting,
//! style classification and enrichment from Hyperliquid userFills.

use std::collections::{HashMap, VecDeque};

use shared::{Action, Intent, Side, conviction_tag};

/// Live position tracker for one (trader, coin).
#[derive(Clone, Debug, Default)]
pub struct PosTrack {
    /// signed size (base units)
    pub size: f64,
    pub avg_px: f64,
    pub opened_at: i64,
    pub last_t: i64,
}

impl PosTrack {
    /// Apply a fill and return (action, realized_pnl).
    /// `qty` is signed: + = buy, - = sell.
    pub fn apply(&mut self, qty: f64, px: f64, t: i64) -> (Action, f64) {
        let mut rpn = 0.0;
        if self.size.abs() < 1e-12 {
            // opening fresh
            self.size = qty;
            self.avg_px = px;
            self.opened_at = t;
            let action = if qty.abs() > 0.0 { Action::Open } else { Action::Flat };
            self.last_t = t;
            return (action, 0.0);
        }
        let same_sign = self.size * qty > 0.0;
        if same_sign {
            // increase
            let new_sz = self.size + qty;
            self.avg_px = (self.avg_px * self.size + px * qty) / new_sz.abs();
            self.size = new_sz;
            self.last_t = t;
            return (Action::Increase, 0.0);
        }
        // reducing or flipping
        let closed = qty.abs().min(self.size.abs());
        if self.size > 0.0 {
            rpn = (px - self.avg_px) * closed;
        } else {
            rpn = (self.avg_px - px) * closed;
        }
        let new_sz = self.size + qty;
        if new_sz.abs() < 1e-12 {
            self.size = 0.0;
            self.avg_px = 0.0;
            self.last_t = t;
            return (Action::Reduce, rpn);
        }
        if new_sz * self.size > 0.0 {
            // still same sign -> reduce
            self.size = new_sz;
            self.last_t = t;
            return (Action::Reduce, rpn);
        }
        // flipped
        self.size = new_sz;
        self.avg_px = px;
        self.opened_at = t;
        self.last_t = t;
        (Action::Flip, rpn)
    }
}

/// Enrichment pulled from Hyperliquid REST (userFills / clearinghouseState).
#[derive(Clone, Debug)]
pub struct Enriched {
    /// sum of closedPnl over the last (up to 2000) fills
    pub pnl: f64,
    pub fills_n: u32,
    pub win: u32,
    pub loss: u32,
    /// average hold seconds (open -> close)
    pub avg_hold: f64,
    /// fraction of fills where this wallet was taker
    pub taker_ratio: f64,
    pub account_value: Option<f64>,
    pub median_size_usd: f64,
    pub updated: i64,
    /// fills in the recent window that carried a liquidation marker
    pub liq_fills: u32,
    /// exact position per coin from the last fill's startPosition
    pub positions: HashMap<String, f64>,
}

impl Enriched {
    pub fn win_rate(&self) -> Option<f64> {
        let n = self.win + self.loss;
        if n == 0 {
            None
        } else {
            Some(self.win as f64 / n as f64)
        }
    }
}

/// Live observed state of one wallet.
pub struct TraderState {
    pub addr: String,
    pub positions: HashMap<String, PosTrack>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub prints: u32,
    pub liq_count: u32,
    /// session observed realized pnl
    pub session_rpn: f64,
    /// last 64 realized pnl events (for win rate)
    pub pnl_events: VecDeque<f64>,
    /// recent fills (t, coin, is_buy, usd) for hedge detection
    pub recent: VecDeque<(i64, String, bool, f64)>,
    /// observed hold durations (sec)
    pub holds: VecDeque<f64>,
    pub enriched: Option<Enriched>,
    /// label override (known wallets)
    pub label: Option<String>,
}

impl TraderState {
    pub fn new(addr: String, t: i64) -> Self {
        Self {
            addr,
            positions: HashMap::new(),
            first_seen: t,
            last_seen: t,
            prints: 0,
            liq_count: 0,
            session_rpn: 0.0,
            pnl_events: VecDeque::new(),
            recent: VecDeque::new(),
            holds: VecDeque::new(),
            enriched: None,
            label: None,
        }
    }

    pub fn push_recent(&mut self, t: i64, coin: &str, is_buy: bool, usd: f64) {
        self.recent.push_back((t, coin.to_string(), is_buy, usd));
        while self.recent.len() > 64 {
            self.recent.pop_front();
        }
    }

    /// Net style tag: "++", "+", "±", "-", "--" based on pnl + win rate.
    pub fn conviction(&self) -> String {
        let (pnl, wr) = match &self.enriched {
            Some(e) => (e.pnl, e.win_rate()),
            None => (self.session_rpn, None),
        };
        let pnl_score = (pnl / 10_000.0).tanh();
        let wr_score = wr.map(|r| (r - 0.5) * 2.0).unwrap_or(0.0);
        let score = 0.6 * pnl_score + 0.4 * wr_score;
        conviction_tag(score).to_string()
    }

    /// Style label from blended enriched + observed data.
    pub fn style(&self) -> String {
        if let Some(l) = &self.label {
            return l.to_uppercase();
        }
        let (hold, n_fills, taker_ratio) = match &self.enriched {
            Some(e) => (
                if e.avg_hold > 0.0 {
                    Some(e.avg_hold)
                } else {
                    self.observed_hold()
                },
                e.fills_n + self.prints,
                Some(e.taker_ratio),
            ),
            None => (self.observed_hold(), self.prints, None),
        };
        if taker_ratio.map(|r| r < 0.15).unwrap_or(false) && n_fills > 20 {
            return "MM".into();
        }
        if n_fills < 6 {
            return "NEW".into();
        }
        match hold {
            Some(h) if h < 180.0 => "SCALPER".into(),
            Some(h) if h < 3_600.0 => "INTRADAY".into(),
            Some(h) if h < 28_800.0 => "DAY".into(),
            Some(h) if h < 259_200.0 => "SWING".into(),
            Some(_) => "POSITION".into(),
            None => "INTRADAY".into(),
        }
    }

    fn observed_hold(&self) -> Option<f64> {
        if self.holds.is_empty() {
            None
        } else {
            let mut v: Vec<f64> = self.holds.iter().copied().collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            Some(v[v.len() / 2])
        }
    }

    pub fn win_rate(&self) -> Option<f64> {
        if let Some(e) = &self.enriched {
            if let Some(r) = e.win_rate() {
                return Some(r);
            }
        }
        if self.pnl_events.len() >= 4 {
            let wins = self.pnl_events.iter().filter(|p| **p > 0.0).count();
            return Some(wins as f64 / self.pnl_events.len() as f64);
        }
        None
    }

    pub fn pnl(&self) -> Option<f64> {
        match &self.enriched {
            Some(e) => Some(e.pnl),
            None => {
                if self.pnl_events.is_empty() && self.session_rpn == 0.0 {
                    None
                } else {
                    Some(self.session_rpn)
                }
            }
        }
    }
}

/// Lightweight immutable snapshot used when rendering.
#[derive(Clone, Debug)]
pub struct TraderSnapshot {
    pub style: String,
    pub tag: String,
    pub pnl: Option<f64>,
    pub win_rate: Option<f64>,
    pub account_value: Option<f64>,
    pub label: Option<String>,
    pub first_seen: i64,
    pub liq_count: u32,
}

impl TraderSnapshot {
    pub fn from_state(t: &TraderState) -> Self {
        Self {
            style: t.style(),
            tag: t.conviction(),
            pnl: t.pnl(),
            win_rate: t.win_rate(),
            account_value: t.enriched.as_ref().and_then(|e| e.account_value),
            label: t.label.clone(),
            first_seen: t.first_seen,
            liq_count: t.liq_count,
        }
    }
}

// ---------------------------------------------------------------------------
// Intent engine
// ---------------------------------------------------------------------------

/// Classify the intent of a taker fill given the action + realized pnl.
pub fn classify_intent(action: Action, rpn: f64, usd: f64, pending_liq: bool, side: Side) -> Intent {
    if pending_liq {
        return Intent::Liquidation;
    }
    match action {
        Action::Open | Action::Increase => Intent::Aggressive,
        Action::Reduce | Action::Flip => {
            let rel = if usd > 0.0 { rpn.abs() / usd } else { 0.0 };
            if rpn < 0.0 && rel > 0.0005 {
                Intent::StopLoss
            } else if rpn > 0.0 && rel > 0.0005 {
                Intent::TakeProfit
            } else {
                Intent::RiskReduce
            }
        }
        Action::Flat => {
            if side.is_buy() {
                Intent::Aggressive
            } else {
                Intent::Aggressive
            }
        }
    }
}

/// Check if this fill hedge-matches one of the trader's recent fills in a
/// *different* coin (opposite direction, similar notional, close in time).
pub fn hedge_match(recent: &VecDeque<(i64, String, bool, f64)>, coin: &str, is_buy: bool, usd: f64, now: i64) -> bool {
    for (t, c, b, u) in recent.iter().rev().take(24) {
        if c != coin
            && *b != is_buy
            && now - *t < 45_000
            && *u > 500.0
            && usd > 500.0
        {
            let ratio = if *u > 0.0 { usd / *u } else { 0.0 };
            if ratio > 0.6 && ratio < 1.67 {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Enrichment computation from userFills
// ---------------------------------------------------------------------------

pub fn compute_enriched(
    fills: &[crate::hl::types::HlUserFill],
    account_value: Option<f64>,
    now_ms: i64,
) -> Option<Enriched> {
    if fills.is_empty() {
        return None;
    }
    let mut pnl = 0.0;
    let (mut win, mut loss) = (0u32, 0u32);
    let mut hold_sum = 0.0;
    let mut hold_n = 0u32;
    let mut crossed_n = 0u32;
    let mut sizes: Vec<f64> = Vec::with_capacity(fills.len());
    let mut liq_fills = 0u32;
    // last open time + price per coin, to estimate holds
    let mut last_open: HashMap<String, (i64, f64)> = HashMap::new();
    let mut positions: HashMap<String, f64> = HashMap::new();

    for f in fills.iter() {
        let px: f64 = f.px.parse().unwrap_or(0.0);
        let sz: f64 = f.sz.parse().unwrap_or(0.0);
        let closed_pnl: f64 = f.closed_pnl.parse().unwrap_or(0.0);
        pnl += closed_pnl;
        sizes.push(px * sz);
        if f.crossed == Some(true) {
            crossed_n += 1;
        }
        if f.liquidation.is_some() {
            liq_fills += 1;
        }
        let is_close = f.dir.starts_with("Close") || f.dir.contains(">");
        if is_close && closed_pnl != 0.0 {
            if closed_pnl > 0.0 {
                win += 1;
            } else {
                loss += 1;
            }
            if let Some((t0, _)) = last_open.get(&f.coin) {
                let h = (f.time - t0) as f64 / 1000.0;
                if h > 0.0 && h < 30.0 * 86400.0 {
                    hold_sum += h;
                    hold_n += 1;
                }
            }
        } else if !is_close {
            last_open.insert(f.coin.clone(), (f.time, px));
        }
        // track final position per coin from startPosition of the newest fill
        if let Ok(sp) = f.start_position.parse::<f64>() {
            positions.insert(f.coin.clone(), sp);
        }
    }
    sizes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_size = sizes.get(sizes.len() / 2).copied().unwrap_or(0.0);

    Some(Enriched {
        pnl,
        fills_n: fills.len() as u32,
        win,
        loss,
        avg_hold: if hold_n > 0 { hold_sum / hold_n as f64 } else { 0.0 },
        taker_ratio: crossed_n as f64 / fills.len() as f64,
        account_value,
        median_size_usd: median_size,
        updated: now_ms,
        liq_fills,
        positions,
    })
}
