//! Per-coin market state: raw trade tape, candles, liquidations,
//! plus on-demand footprint / level-detail / trader aggregation.

use std::collections::{BTreeMap, HashMap, VecDeque};

use shared::{
    bucket_center, bucket_idx, bucket_size, window_start, Action, ActionSplit, Candle,
    CoinStats, FootWindow, FootprintSnapshot, Intent, LevelAgg, LevelDetail, Side, WalletRow,
    WindowTrader,
};

pub const MAX_TRADES: usize = 150_000;
pub const MAX_LIQS: usize = 5_000;
pub const MAX_CANDLES: usize = 60 * 60; // 60h of 1m

/// A stored raw fill as observed on-chain.
#[derive(Clone, Debug)]
pub struct StoredTrade {
    pub t: i64,
    pub px: f64,
    pub sz: f64,
    pub usd: f64,
    /// taker side
    pub side: Side,
    pub taker: String,
    pub maker: String,
    pub intent: Intent,
    pub action: Action,
    /// realized pnl for the taker if reducing
    pub rpn: f64,
}

#[derive(Clone, Debug)]
pub struct StoredLiq {
    pub t: i64,
    pub user: String,
    pub sz: f64,
    pub usd: f64,
    pub px: f64,
    pub long: bool,
}

pub struct CoinInner {
    pub trades: VecDeque<StoredTrade>,
    pub candles: BTreeMap<i64, Candle>,
    pub liqs: VecDeque<StoredLiq>,
    pub tick_est: f64,
    pub last_px: f64,
    pub sub_refcount: usize,
    pub last_client_at: i64,
}

pub struct CoinState {
    pub coin: String,
    pub inner: std::sync::Mutex<CoinInner>,
}

impl CoinState {
    pub fn new(coin: String, tick: f64) -> Self {
        Self {
            coin,
            inner: std::sync::Mutex::new(CoinInner {
                trades: VecDeque::new(),
                candles: BTreeMap::new(),
                liqs: VecDeque::new(),
                tick_est: tick,
                last_px: 0.0,
                sub_refcount: 0,
                last_client_at: 0,
            }),
        }
    }
}

/// Infer tick size from observed trade prices (min positive diff, floored).
pub fn infer_tick(prices: &mut Vec<f64>) -> f64 {
    prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    prices.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON * 100.0);
    let mut min_diff = f64::MAX;
    for w in prices.windows(2) {
        let d = w[1] - w[0];
        if d > 0.0 && d < min_diff {
            min_diff = d;
        }
    }
    if min_diff == f64::MAX || min_diff <= 0.0 {
        0.0
    } else {
        min_diff
    }
}

// ---------------------------------------------------------------------------
// Footprint snapshot
// ---------------------------------------------------------------------------

struct WinBuilder {
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    first: bool,
    levels: HashMap<i64, LevelAgg>,
    wallets: HashMap<String, WalletAgg>,
    prints: u32,
    buy: f64,
    sell: f64,
}

#[derive(Default, Clone)]
struct WalletAgg {
    b: f64,
    s: f64,
    rpn_b: f64, // realized pnl of reducing buyers (buying to close shorts)
    rpn_s: f64,
    actions: ActionSplit,
    maker_b: f64,
    maker_s: f64,
    liq: bool,
}

impl Default for WinBuilder {
    fn default() -> Self {
        WinBuilder {
            o: 0.0,
            h: 0.0,
            l: f64::MAX,
            c: 0.0,
            first: true,
            levels: HashMap::new(),
            wallets: HashMap::new(),
            prints: 0,
            buy: 0.0,
            sell: 0.0,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_footprint(
    coin: &str,
    inner: &CoinInner,
    interval_ms: i64,
    limit: usize,
    bucket_mult: f64,
    now_ms: i64,
    trader_snap: &dyn Fn(&str) -> Option<crate::engine::traders::TraderSnapshot>,
) -> FootprintSnapshot {
    let limit = limit.clamp(10, 400);
    let cur_win = window_start(now_ms, interval_ms);
    let win_lo = cur_win - (limit as i64 - 1) * interval_ms;

    let mut mark = inner.last_px;
    if mark <= 0.0 {
        if let Some(c) = inner.candles.values().next_back() {
            mark = c.c;
        }
    }
    let tick = if inner.tick_est > 0.0 { inner.tick_est } else { 10f64.powi(mark.abs().log10().floor() as i32 - 4) };
    let bucket = bucket_size(mark, bucket_mult, tick);

    let mut windows: BTreeMap<i64, WinBuilder> = BTreeMap::new();
    for tr in inner.trades.iter().filter(|t| t.t >= win_lo) {
        let w = window_start(tr.t, interval_ms);
        let wb = windows.entry(w).or_default();
        if wb.first {
            wb.o = tr.px;
            wb.first = false;
        }
        wb.c = tr.px;
        wb.h = wb.h.max(tr.px);
        wb.l = wb.l.min(tr.px);
        wb.prints += 1;
        let bi = bucket_idx(tr.px, bucket);
        let cell = wb.levels.entry(bi).or_insert_with(|| LevelAgg {
            p: bucket_center(bi, bucket),
            ..Default::default()
        });
        let buy = tr.side.is_buy();
        if buy {
            cell.buy += tr.usd;
            wb.buy += tr.usd;
        } else {
            cell.sell += tr.usd;
            wb.sell += tr.usd;
        }
        cell.n += 1;
        match tr.intent {
            Intent::Liquidation | Intent::Deleveraging => cell.liq += tr.usd,
            Intent::StopLoss => cell.stop += tr.usd,
            Intent::TakeProfit => cell.tp += tr.usd,
            Intent::Hedge => cell.hedge += tr.usd,
            _ => {}
        }

        // taker wallet
        let wcell = wb.wallets.entry(tr.taker.clone()).or_default();
        if buy {
            wcell.b += tr.usd;
        } else {
            wcell.s += tr.usd;
        }
        match tr.action {
            Action::Open => wcell.actions.open += tr.usd,
            Action::Increase => wcell.actions.increase += tr.usd,
            Action::Reduce => wcell.actions.reduce += tr.usd,
            Action::Flip => wcell.actions.flip += tr.usd,
            _ => {}
        }
        if buy {
            wcell.rpn_b += tr.rpn;
        } else {
            wcell.rpn_s += tr.rpn;
        }
        if matches!(tr.intent, Intent::Liquidation | Intent::Deleveraging) {
            wcell.liq = true;
        }
        // maker wallet (resting side)
        let mcell = wb.wallets.entry(tr.maker.clone()).or_default();
        if buy {
            mcell.maker_s += tr.usd; // taker bought from maker (maker sold)
        } else {
            mcell.maker_b += tr.usd;
        }
    }

    // OHLC backfill from candles for windows that have no trades
    let mut candles: Vec<Candle> = Vec::new();
    {
        let mut by_c: BTreeMap<i64, Candle> = BTreeMap::new();
        for (_, c) in inner.candles.range(win_lo..) {
            let w = window_start(c.t, interval_ms);
            let e = by_c.entry(w).or_default();
            if e.t == 0 {
                e.t = w;
                e.o = c.o;
                e.h = c.h;
                e.l = c.l;
            } else {
                e.merge(c);
            }
            // merge sub-candles properly (o of first, c of last)
            e.c = c.c;
            e.h = e.h.max(c.h);
            e.l = e.l.min(c.l);
            e.v += c.v;
            e.n += c.n;
        }
        candles = by_c.into_values().collect();
    }

    let mut out_windows: Vec<FootWindow> = Vec::with_capacity(windows.len());
    for (w, wb) in windows {
        // merge candle ohlc if available
        let (mut o, mut h, mut l, mut c) = (wb.o, wb.h, wb.l, wb.c);
        let ch = candles
            .iter()
            .find(|cc| cc.t == w)
            .or_else(|| candles.iter().find(|cc| window_start(cc.t, interval_ms) == w));
        if let Some(cc) = ch {
            if o == 0.0 {
                o = cc.o;
            }
            h = h.max(cc.h);
            l = l.min(cc.l);
            c = cc.c;
        }
        let mut poc = 0.0;
        let mut poc_vol = -1.0;
        let mut levels: Vec<LevelAgg> = wb.levels.into_values().collect();
        levels.sort_by(|a, b| a.p.partial_cmp(&b.p).unwrap_or(std::cmp::Ordering::Equal));
        for lv in levels.iter() {
            let tot = lv.total();
            if tot > poc_vol {
                poc_vol = tot;
                poc = lv.p;
            }
        }
        // top buyers / sellers (aggressor volume only)
        let mut wl: Vec<(String, WalletAgg)> = wb.wallets.into_iter().collect();
        let total_vol: f64 = wl.iter().map(|(_, a)| a.b + a.s).sum();
        let mut tb: Vec<WindowTrader> = Vec::new();
        let mut ts: Vec<WindowTrader> = Vec::new();
        wl.sort_by(|a, b| b.1.b.partial_cmp(&a.1.b).unwrap_or(std::cmp::Ordering::Equal));
        for (addr, agg) in wl.iter().take(6) {
            if agg.b <= 0.0 {
                continue;
            }
            let snap = trader_snap(addr);
            tb.push(WindowTrader {
                a: addr.clone(),
                b: agg.b,
                s: 0.0,
                tag: snap.as_ref().map(|s| s.tag.clone()).unwrap_or_else(|| "±".into()),
                pnl: snap.as_ref().and_then(|s| s.pnl),
                liq: agg.liq,
            });
        }
        wl.sort_by(|a, b| b.1.s.partial_cmp(&a.1.s).unwrap_or(std::cmp::Ordering::Equal));
        for (addr, agg) in wl.iter().take(6) {
            if agg.s <= 0.0 {
                continue;
            }
            let snap = trader_snap(addr);
            ts.push(WindowTrader {
                a: addr.clone(),
                b: 0.0,
                s: agg.s,
                tag: snap.as_ref().map(|s| s.tag.clone()).unwrap_or_else(|| "±".into()),
                pnl: snap.as_ref().and_then(|s| s.pnl),
                liq: agg.liq,
            });
        }
        let _ = total_vol;
        out_windows.push(FootWindow {
            t: w,
            o,
            h,
            l,
            c,
            buy: wb.buy,
            sell: wb.sell,
            delta: wb.buy - wb.sell,
            poc,
            wallets: wl.len() as u32,
            prints: wb.prints,
            levels,
            tb,
            ts,
        });
    }

    FootprintSnapshot {
        coin: coin.to_string(),
        interval_ms,
        bucket,
        bucket_mult,
        tick,
        candles,
        windows: out_windows,
        generated_at: now_ms,
    }
}

// ---------------------------------------------------------------------------
// Level detail
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn build_level_detail(
    coin: &str,
    inner: &CoinInner,
    t0: i64,
    t1: i64,
    p_lo: f64,
    p_hi: f64,
    bucket: f64,
    now_ms: i64,
    trader_snap: &dyn Fn(&str) -> Option<crate::engine::traders::TraderSnapshot>,
) -> LevelDetail {
    let mut rows: HashMap<String, WalletRowBuilder> = HashMap::new();
    let mut buy = 0.0;
    let mut sell = 0.0;
    let mut prints = 0u32;
    let mut aggressor = 0.0;
    let mut resting = 0.0;
    let mut actions = ActionSplit::default();
    let mut rpn_buyers = 0.0;
    let mut rpn_sellers = 0.0;
    let mut liq_usd = 0.0;

    for tr in inner
        .trades
        .iter()
        .filter(|t| t.t >= t0 && t.t < t1 && bucket_idx(t.px, bucket) == bucket_idx(p_lo, bucket))
    {
        prints += 1;
        let buy_side = tr.side.is_buy();
        if buy_side {
            buy += tr.usd;
            aggressor += tr.usd;
        } else {
            sell += tr.usd;
            aggressor += tr.usd;
        }
        if matches!(tr.intent, Intent::Liquidation | Intent::Deleveraging) {
            liq_usd += tr.usd;
        }
        // taker row
        {
            let row = rows.entry(tr.taker.clone()).or_default();
            row.is_maker = false;
            if buy_side {
                row.bought += tr.usd;
                rpn_buyers += tr.rpn;
                row.rpn += tr.rpn;
            } else {
                row.sold += tr.usd;
                rpn_sellers += tr.rpn;
                row.rpn += tr.rpn;
            }
            match tr.action {
                Action::Open => actions.open += tr.usd,
                Action::Increase => actions.increase += tr.usd,
                Action::Reduce => actions.reduce += tr.usd,
                Action::Flip => actions.flip += tr.usd,
                _ => {}
            }
            if matches!(tr.intent, Intent::Liquidation | Intent::Deleveraging) {
                row.liq = true;
            }
        }
        // maker row (resting)
        {
            let row = rows.entry(tr.maker.clone()).or_default();
            if buy_side {
                row.sold += tr.usd;
                row.maker_s += tr.usd;
            } else {
                row.bought += tr.usd;
                row.maker_b += tr.usd;
            }
            resting += tr.usd;
        }
    }

    let total: f64 = rows.values().map(|r| r.bought + r.sold).sum();
    let mut rows_v: Vec<WalletRow> = rows
        .into_iter()
        .map(|(a, r)| {
            let snap = trader_snap(&a);
            let share = if total > 0.0 { (r.bought + r.sold) / total } else { 0.0 };
            WalletRow {
                a,
                label: snap.as_ref().and_then(|s| s.label.clone()),
                style: snap
                    .as_ref()
                    .map(|s| s.style.clone())
                    .unwrap_or_else(|| "NEW".into()),
                tag: snap.as_ref().map(|s| s.tag.clone()).unwrap_or_else(|| "±".into()),
                bought: r.bought,
                sold: r.sold,
                net: r.bought - r.sold,
                share,
                rpn: r.rpn,
                pnl_all: snap.as_ref().and_then(|s| s.pnl),
                win_rate: snap.as_ref().and_then(|s| s.win_rate),
                liq: r.liq,
                is_maker: r.maker_b + r.maker_s > r.bought + r.sold - r.maker_b - r.maker_s,
            }
        })
        .collect();
    rows_v.sort_by(|a, b| (b.bought + b.sold).partial_cmp(&(a.bought + a.sold)).unwrap_or(std::cmp::Ordering::Equal));

    let mut detail = LevelDetail {
        coin: coin.to_string(),
        t_start: t0,
        t_end: t1,
        p_lo,
        p_hi,
        buy_usd: buy,
        sell_usd: sell,
        delta: buy - sell,
        prints,
        wallets: rows_v.len() as u32,
        aggressor_usd: aggressor,
        resting_usd: resting,
        actions,
        rpn_buyers,
        rpn_sellers,
        liq_usd,
        wallets_rows: rows_v,
    };
    // clamp an ongoing window's end marker to now
    if detail.t_end > now_ms {
        detail.t_end = now_ms;
    }
    detail
}

#[derive(Default)]
struct WalletRowBuilder {
    bought: f64,
    sold: f64,
    maker_b: f64,
    maker_s: f64,
    rpn: f64,
    liq: bool,
    is_maker: bool,
}

// ---------------------------------------------------------------------------
// Rolling stats
// ---------------------------------------------------------------------------

pub fn build_stats(coin: &str, inner: &CoinInner, window_ms: i64, now_ms: i64) -> CoinStats {
    let lo = now_ms - window_ms;
    let mut s = CoinStats {
        coin: coin.to_string(),
        ..Default::default()
    };
    for tr in inner.trades.iter().filter(|t| t.t >= lo) {
        if tr.side.is_buy() {
            s.buy += tr.usd;
        } else {
            s.sell += tr.usd;
        }
        s.prints += 1;
        match tr.intent {
            Intent::Liquidation | Intent::Deleveraging => {
                s.liq_count += 1;
                s.liq_usd += tr.usd;
            }
            Intent::StopLoss => s.stop_usd += tr.usd,
            Intent::TakeProfit => s.tp_usd += tr.usd,
            Intent::Hedge => s.hedge_usd += tr.usd,
            _ => {}
        }
    }
    s.delta = s.buy - s.sell;
    s
}

/// Recent liquidation feed for a coin.
pub fn recent_liqs(inner: &CoinInner, limit: usize) -> Vec<StoredLiq> {
    inner.liqs.iter().rev().take(limit).cloned().collect()
}
