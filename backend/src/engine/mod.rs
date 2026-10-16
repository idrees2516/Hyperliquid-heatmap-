//! Engine: central application state + trade/liquidation/candle processing.

pub mod market;
pub mod opps;
pub mod persist;
pub mod radar;
pub mod tasks;
pub mod traders;
pub mod tuning;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use shared::{AssetInfo, Candle, Intent, LiveLiquidation, LiveTrade, TraderRow};
use tokio::sync::{broadcast, mpsc};

use crate::engine::traders::TraderSnapshot;
use crate::hl::types::{HlLiqInner, HlWsTrade};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Markets tracked even when the assets REST endpoint is rate-limited / down,
/// so the realtime pipeline always has flow. Extended majors set.
/// All names must exist in Hyperliquid's perp universe: subscribing to a
/// delisted/unknown coin makes the upstream reset the whole socket.
pub const FALLBACK_COINS: &[&str] = &[
    "BTC", "ETH", "SOL", "HYPE", "XRP", "DOGE", "BNB", "ADA", "AVAX", "LINK",
    "TRX", "SUI", "LTC", "ZEC", "ARB", "OP", "NEAR", "APT", "TIA", "WIF",
];

/// Commands to the upstream subscription manager.
#[derive(Clone, Debug)]
pub enum SubCmd {
    AddCoin(String),
    RemoveCoin(String),
}

/// Events broadcast to browser clients.
#[derive(Clone, Debug)]
pub enum EngineEvent {
    Assets(Vec<AssetInfo>),
    Trade(LiveTrade),
    Liquidation(LiveLiquidation),
    Candle { coin: String, candle: Candle },
    Traders { coin: String, rows: Vec<TraderRow> },
    Radar { coin: String, snapshot: shared::RadarSnapshot },
    Opps(Vec<shared::Opportunity>),
    Tuning(shared::TuningParams),
    Status { upstream: bool },
}

pub struct AppState {
    pub assets: RwLock<Vec<AssetInfo>>,
    pub coins: RwLock<HashMap<String, Arc<market::CoinState>>>,
    pub traders: RwLock<HashMap<String, Arc<Mutex<traders::TraderState>>>>,
    /// liquidation radar per coin (persisted)
    pub radar: RwLock<HashMap<String, Arc<Mutex<radar::RadarState>>>>,
    /// global recent cascade records (persisted), newest last
    pub cascades: Mutex<VecDeque<shared::CascadeRecord>>,
    pub broadcast: broadcast::Sender<EngineEvent>,
    pub sub_cmds: mpsc::UnboundedSender<SubCmd>,
    pub rest: crate::hl::rest::HlRest,
    pub started_at: i64,
    pub upstream_alive: RwLock<bool>,
    pub default_coins: RwLock<Vec<String>>,
    /// live-tunable engine parameters (persisted)
    pub tuning: Arc<tuning::Tuning>,
    pub persist: persist::Persist,
    pub data_dir: PathBuf,
    /// pending liquidations awaiting their matching trade
    pending_liq: Mutex<HashMap<(String, String), (i64, bool)>>,
}

impl AppState {
    pub fn new(rest: crate::hl::rest::HlRest, sub_cmds: mpsc::UnboundedSender<SubCmd>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(8192);
        let data_dir = std::env::var("FOOTPRINT_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("data"));
        let persist = persist::Persist::new(&data_dir);
        let tuning = Arc::new(tuning::Tuning::load_or_default(&data_dir));
        // restore radar + cascade history so overhang signals survive restarts
        let mut radar_map: HashMap<String, Arc<Mutex<radar::RadarState>>> = HashMap::new();
        for pr in persist.load_radar() {
            let coin = pr.coin.clone();
            radar_map.insert(coin, Arc::new(Mutex::new(radar::RadarState::from_persist(pr))));
        }
        if !radar_map.is_empty() {
            tracing::info!(coins = radar_map.len(), "restored radar levels from disk");
        }
        let cascades = persist.load_cascades();
        if !cascades.is_empty() {
            tracing::info!(n = cascades.len(), "restored cascade history from disk");
        }
        Arc::new(Self {
            assets: RwLock::new(Vec::new()),
            coins: RwLock::new(HashMap::new()),
            traders: RwLock::new(HashMap::new()),
            radar: RwLock::new(radar_map),
            cascades: Mutex::new(cascades),
            broadcast: tx,
            sub_cmds,
            rest,
            started_at: now_ms(),
            upstream_alive: RwLock::new(false),
            default_coins: RwLock::new(Vec::new()),
            tuning,
            persist,
            data_dir,
            pending_liq: Mutex::new(HashMap::new()),
        })
    }

    pub fn get_or_create_coin(&self, coin: &str) -> Arc<market::CoinState> {
        if let Some(c) = self.coins.read().unwrap().get(coin) {
            return c.clone();
        }
        let mut w = self.coins.write().unwrap();
        w.entry(coin.to_string())
            .or_insert_with(|| {
                tracing::info!("tracking coin {coin}");
                Arc::new(market::CoinState::new(coin.to_string(), 0.0))
            })
            .clone()
    }

    pub fn get_coin(&self, coin: &str) -> Option<Arc<market::CoinState>> {
        self.coins.read().unwrap().get(coin).cloned()
    }

    pub fn get_or_create_radar(&self, coin: &str) -> Arc<Mutex<radar::RadarState>> {
        if let Some(r) = self.radar.read().unwrap().get(coin) {
            return r.clone();
        }
        let mut w = self.radar.write().unwrap();
        w.entry(coin.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(radar::RadarState::new(coin))))
            .clone()
    }

    pub fn get_radar(&self, coin: &str) -> Option<Arc<Mutex<radar::RadarState>>> {
        self.radar.read().unwrap().get(coin).cloned()
    }

    /// May we subscribe to this coin upstream? Subscribing to a coin that
    /// is not in Hyperliquid's live perp universe makes the upstream reset
    /// the whole WebSocket connection, so guard every subscription. While
    /// the universe is still loading (empty assets), only the static
    /// fallback set is considered tradable.
    pub fn coin_tradable(&self, coin: &str) -> bool {
        let assets = self.assets.read().unwrap();
        if assets.is_empty() {
            return FALLBACK_COINS.iter().any(|c| *c == coin);
        }
        assets.iter().any(|a| a.name == coin)
    }

    /// Register a freshly finalized cascade into the global history.
    pub fn push_cascade(&self, rec: shared::CascadeRecord) {
        let mut q = self.cascades.lock().unwrap();
        q.push_back(rec);
        while q.len() > persist::MAX_GLOBAL_CASCADES {
            q.pop_front();
        }
    }

    fn get_trader(&self, addr: &str, t: i64) -> Arc<Mutex<traders::TraderState>> {
        if let Some(x) = self.traders.read().unwrap().get(addr) {
            return x.clone();
        }
        let mut w = self.traders.write().unwrap();
        w.entry(addr.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(traders::TraderState::new(addr.to_string(), t))))
            .clone()
    }

    pub fn trader_snapshot(&self, addr: &str) -> Option<TraderSnapshot> {
        let t = self.traders.read().unwrap().get(addr)?.clone();
        let guard = t.lock().unwrap();
        Some(TraderSnapshot::from_state(&guard))
    }

    // -----------------------------------------------------------------------
    // Trade processing
    // -----------------------------------------------------------------------

    pub fn process_trade(&self, raw: &HlWsTrade) {
        let Some(side) = raw.side() else { return };
        let px: f64 = match raw.px.parse() {
            Ok(p) => p,
            Err(_) => return,
        };
        let sz: f64 = match raw.sz.parse() {
            Ok(s) => s,
            Err(_) => return,
        };
        if !px.is_finite() || !sz.is_finite() || sz <= 0.0 {
            return;
        }
        let taker = raw.users.first().cloned().unwrap_or_default();
        let maker = raw.users.get(1).cloned().unwrap_or_default();
        if taker.is_empty() {
            return;
        }
        let usd = px * sz;
        let now = raw.time;
        let params = self.tuning.get();

        // sweep slippage vs the price ~30s ago (measures how deep this print
        // went through the book, relative to recent trading)
        let ref_px = self.px_lag(&raw.coin, now, 30_000);
        let slip_bps = if ref_px > 0.0 {
            let raw_slip = if side.is_buy() {
                (px - ref_px) / ref_px
            } else {
                (ref_px - px) / ref_px
            };
            (raw_slip * 10_000.0).max(0.0)
        } else {
            0.0
        };

        // 1. position / intent for taker
        let (action, rpn, pending, hedge, pos_before_abs, pos_after_abs, burst) = {
            let ts = self.get_trader(&taker, now);
            let mut g = ts.lock().unwrap();
            g.last_seen = now;
            g.prints += 1;
            let opened_before = {
                let pos = g
                    .positions
                    .entry(raw.coin.clone())
                    .or_insert_with(|| traders::PosTrack::default());
                let pos_before_abs = pos.size.abs();
                let opened_at = pos.opened_at;
                let (action, rpn) = pos.apply(side.sign() * sz, px, now);
                let pos_after_abs = pos.size.abs();
                (action, rpn, opened_at, pos_before_abs, pos_after_abs)
            };
            let (action, rpn, opened_at, pos_before_abs, pos_after_abs) = opened_before;
            if rpn.abs() > 1e-9 {
                g.session_rpn += rpn;
                g.pnl_events.push_back(rpn);
                if g.pnl_events.len() > 64 {
                    g.pnl_events.pop_front();
                }
                let hold = (now - opened_at.max(g.first_seen)) as f64 / 1000.0;
                if hold > 0.0 && hold < 30.0 * 86400.0 {
                    g.holds.push_back(hold);
                    if g.holds.len() > 64 {
                        g.holds.pop_front();
                    }
                }
            }
            let pending = {
                let mut pl = self.pending_liq.lock().unwrap();
                pl.remove(&(raw.coin.clone(), taker.clone()))
                    .map(|(_, long)| long)
                    .unwrap_or(false)
            };
            let hedge = traders::hedge_match(&g.recent, &raw.coin, side.is_buy(), usd, now);
            // burst: same-direction aggressive prints by this wallet in ~3s
            let burst = 1 + g
                .recent
                .iter()
                .rev()
                .take(24)
                .filter(|(t, c, b, _)| *c == raw.coin && *b == side.is_buy() && now - *t <= 3_000)
                .count() as u32;
            g.push_recent(now, &raw.coin, side.is_buy(), usd);
            (action, rpn, pending, hedge, pos_before_abs, pos_after_abs, burst)
        };

        let mut intent = traders::classify_intent(action, rpn, usd, pending, side);
        if hedge && matches!(intent, Intent::Aggressive) {
            intent = Intent::Hedge;
        }

        // --- strict behavioral liquidation gate ---------------------------
        // A forced close must look like one: near-full close of a real loss,
        // printed as an aggressive burst that swept the book. Confidence and
        // sweep depth are tunable live (liq_conf_cap / liq_min_sweep_slip_bps).
        let close_frac = if pos_before_abs > 0.0 {
            ((pos_before_abs - pos_after_abs) / pos_before_abs).max(0.0)
        } else {
            0.0
        };
        let loss_bps = if usd > 0.0 { (-rpn).max(0.0) / usd * 10_000.0 } else { 0.0 };
        let signal = tuning::LiqSignal {
            close_frac,
            loss_bps,
            burst,
            slip_bps,
            usd: pos_before_abs * px,
        };
        let verdict = tuning::liq_verdict(&signal, &params, pending);
        let mut liq_forced = false;
        if !pending && verdict.is_liq {
            intent = Intent::Liquidation;
            liq_forced = true;
        } else if pending {
            intent = Intent::Liquidation;
            liq_forced = true;
        }

        // maker position update (opposite side)
        if !maker.is_empty() {
            let ms = self.get_trader(&maker, now);
            let mut g = ms.lock().unwrap();
            g.last_seen = now;
            g.push_recent(now, &raw.coin, !side.is_buy(), usd);
            let pos = g
                .positions
                .entry(raw.coin.clone())
                .or_insert_with(|| traders::PosTrack::default());
            pos.apply(-side.sign() * sz, px, now);
        }

        // 2. store
        let maker_disp = maker.clone();
        let stored = market::StoredTrade {
            t: now,
            px,
            sz,
            usd,
            side,
            taker: taker.clone(),
            maker,
            intent,
            action,
            rpn,
        };
        let coin_state = self.get_or_create_coin(&raw.coin);
        {
            let mut inner = coin_state.inner.lock().unwrap();
            inner.trades.push_back(stored);
            while inner.trades.len() > market::MAX_TRADES
                || (inner.trades.front().map(|f| now - f.t > 6 * 3600_000).unwrap_or(false))
            {
                if inner.trades.pop_front().is_none() {
                    break;
                }
            }
            inner.last_px = px;
            // tick inference from a sample of recent prices
            if inner.tick_est <= 0.0 && inner.trades.len() >= 80 {
                let prices: Vec<f64> = inner.trades.iter().skip(inner.trades.len() - 200).map(|x| x.px).collect();
                let tick = market::infer_tick(&mut prices.clone());
                if tick > 0.0 {
                    inner.tick_est = tick;
                }
            }
        }

        // 3. radar: every aggressive open/increase seeds magnet levels,
        //    every print advances the price/sweep lifecycle
        {
            let radar = self.get_or_create_radar(&raw.coin);
            let mut r = radar.lock().unwrap();
            if matches!(action, shared::Action::Open | shared::Action::Increase)
                && matches!(intent, Intent::Aggressive)
                && usd >= 5_000.0
            {
                r.on_open(px, usd, side.is_buy(), now, &params);
            }
            r.on_price(px, now, &params);
            if liq_forced {
                // forced flow observed: feed the burst aggregator
                r.on_liq_fill(px, usd, !side.is_buy(), &taker, slip_bps, now, &params);
            }
        }
        // pick up freshly finalized cascades into the global history
        self.drain_new_cascades(&raw.coin);

        // 4. behavioral liquidation event (strict-gated burst close)
        if liq_forced {
            let long = !side.is_buy();
            let liq_usd = pos_before_abs * px;
            let coin_state = self.get_or_create_coin(&raw.coin);
            {
                let mut inner = coin_state.inner.lock().unwrap();
                inner.liqs.push_back(market::StoredLiq {
                    t: now,
                    user: taker.clone(),
                    sz: pos_before_abs,
                    usd: liq_usd,
                    px,
                    long,
                });
                while inner.liqs.len() > market::MAX_LIQS {
                    inner.liqs.pop_front();
                }
            }
            {
                let ts = self.get_trader(&taker, now);
                ts.lock().unwrap().liq_count += 1;
            }
            let _ = self.broadcast.send(EngineEvent::Liquidation(LiveLiquidation {
                coin: raw.coin.clone(),
                user: taker.clone(),
                sz: pos_before_abs,
                usd: liq_usd,
                px,
                long,
                t: now,
            }));
            // patch the rest of the burst (engine sweeps arrive as several fills)
            self.retro_tag(&raw.coin, &taker, now, Intent::Liquidation);
        }

        // 5. broadcast
        let (style, pnl, tag) = {
            let ts = self.traders.read().unwrap().get(&taker).cloned();
            match ts {
                Some(t) => {
                    let g = t.lock().unwrap();
                    (g.style(), g.pnl(), g.conviction())
                }
                None => (String::new(), None, String::new()),
            }
        };
        let _ = self.broadcast.send(EngineEvent::Trade(LiveTrade {
            coin: raw.coin.clone(),
            t: now,
            px,
            sz,
            usd,
            side,
            taker: taker.clone(),
            maker: maker_disp,
            intent,
            action,
            rpn,
            taker_style: style,
            taker_pnl: pnl,
            taker_tag: tag,
            taker_label: None,
        }));
    }

    /// Price as of `lag_ms` ago for a coin (from the stored tape).
    fn px_lag(&self, coin: &str, now: i64, lag_ms: i64) -> f64 {
        let Some(cs) = self.get_coin(coin) else { return 0.0 };
        let inner = cs.inner.lock().unwrap();
        let target = now - lag_ms;
        // trades is time-ascending: last trade with t <= target
        let mut lo = 0usize;
        let mut hi = inner.trades.len();
        let mut found = None;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if inner.trades[mid].t <= target {
                found = Some(mid);
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        match found {
            Some(i) => inner.trades[i].px,
            None => inner.trades.front().map(|t| t.px).unwrap_or(0.0),
        }
    }

    /// Move finalized cascade records from a coin's radar into the global list.
    fn drain_new_cascades(&self, coin: &str) {
        let radar = self.get_or_create_radar(coin);
        let drained: Vec<shared::CascadeRecord> = {
            let mut r = radar.lock().unwrap();
            std::mem::take(&mut r.new_cascades).into_iter().collect()
        };
        if !drained.is_empty() {
            for rec in drained {
                tracing::info!(
                    coin = %rec.coin,
                    usd = rec.usd,
                    long = rec.long,
                    "cascade finalized"
                );
                self.push_cascade(rec);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Liquidations (confirmed on-chain events)
    // -----------------------------------------------------------------------

    pub fn process_liquidation(&self, user: &str, liq: &HlLiqInner, t: i64) {
        let szi: f64 = liq.szi.parse().unwrap_or(0.0);
        let px: f64 = liq.px.parse().unwrap_or(0.0);
        let usd = szi.abs() * px;
        // side in liquidation event: direction of the position that got liquidated
        // B = long position liquidated, A = short position liquidated
        let long = matches!(liq.side.as_str(), "B" | "b" | "long" | "Buy" | "buy");
        let params = self.tuning.get();
        // slippage vs the reported mark when available
        let slip_bps = liq
            .mark_px
            .as_ref()
            .and_then(|m| m.parse::<f64>().ok())
            .filter(|m| *m > 0.0)
            .map(|m| {
                let raw = if long { (m - px) / m } else { (px - m) / m };
                (raw * 10_000.0).max(0.0)
            })
            .unwrap_or(0.0);
        let coin_state = self.get_or_create_coin(&liq.coin);
        {
            let mut inner = coin_state.inner.lock().unwrap();
            // dedup: behavioral detector may have already recorded this event
            let dup = inner
                .liqs
                .iter()
                .rev()
                .take(25)
                .any(|l| l.user == *user && (t - l.t).abs() < 5_000);
            if !dup {
                inner.liqs.push_back(market::StoredLiq {
                    t,
                    user: user.to_string(),
                    sz: szi.abs(),
                    usd,
                    px,
                    long,
                });
                while inner.liqs.len() > market::MAX_LIQS {
                    inner.liqs.pop_front();
                }
            }
        }
        // mark trader
        {
            let ts = self.get_trader(user, t);
            let mut g = ts.lock().unwrap();
            g.liq_count += 1;
        }
        // radar: confirmed forced flow at this price
        {
            let radar = self.get_or_create_radar(&liq.coin);
            let mut r = radar.lock().unwrap();
            r.on_liq_fill(px, usd, long, user, slip_bps, t, &params);
            if usd >= params.cascade_min_usd {
                r.sweep_at(px, usd, long, t, &params);
            }
        }
        self.drain_new_cascades(&liq.coin);
        // pending marker so the matching trade (if it comes after) is tagged
        self.pending_liq
            .lock()
            .unwrap()
            .insert((liq.coin.clone(), user.to_string()), (t, long));
        // retro-patch recent stored trades (liq event may arrive after the fill)
        self.retro_tag(&liq.coin, user, t, Intent::Liquidation);
        let _ = self.broadcast.send(EngineEvent::Liquidation(LiveLiquidation {
            coin: liq.coin.clone(),
            user: user.to_string(),
            sz: szi.abs(),
            usd,
            px,
            long,
            t,
        }));
    }

    pub fn process_deleveraging(&self, users: &[String], coin: &str, t: i64) {
        for u in users {
            if u.is_empty() {
                continue;
            }
            self.retro_tag(coin, u, t, Intent::Deleveraging);
            let ts = self.get_trader(u, t);
            ts.lock().unwrap().liq_count += 1;
        }
    }

    /// Patch intent of recent stored trades for a wallet (used by liq/delev).
    fn retro_tag(&self, coin: &str, user: &str, t: i64, intent: Intent) {
        if let Some(cs) = self.get_coin(coin) {
            let mut inner = cs.inner.lock().unwrap();
            let mut patched = 0;
            for tr in inner.trades.iter_mut().rev().take(400) {
                if (t - tr.t).abs() <= 3_000 && tr.taker == *user {
                    tr.intent = intent;
                    patched += 1;
                    if patched > 40 {
                        break;
                    }
                }
                if tr.t < t - 4_000 {
                    break;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Candles
    // -----------------------------------------------------------------------

    pub fn update_candle(&self, coin: &str, c: Candle) {
        let cs = self.get_or_create_coin(coin);
        {
            let mut inner = cs.inner.lock().unwrap();
            inner.candles.entry(c.t).and_modify(|old| {
                old.o = c.o;
                old.c = c.c;
                old.h = c.h;
                old.l = c.l;
                old.v = c.v;
                old.n = c.n;
            }).or_insert(c);
            while inner.candles.len() > market::MAX_CANDLES {
                let k = *inner.candles.keys().next().unwrap();
                inner.candles.remove(&k);
            }
        }
        let _ = self
            .broadcast
            .send(EngineEvent::Candle { coin: coin.to_string(), candle: c });
    }

    pub fn seed_candles(&self, coin: &str, candles: Vec<Candle>) {
        let cs = self.get_or_create_coin(coin);
        let mut inner = cs.inner.lock().unwrap();
        for c in candles {
            inner.candles.insert(c.t, c);
        }
        while inner.candles.len() > market::MAX_CANDLES {
            let k = *inner.candles.keys().next().unwrap();
            inner.candles.remove(&k);
        }
    }

    // -----------------------------------------------------------------------
    // Aggregation helpers
    // -----------------------------------------------------------------------

    pub fn build_traders(&self, coin: &str, minutes: i64) -> Vec<TraderRow> {
        let Some(cs) = self.get_coin(coin) else {
            return Vec::new();
        };
        let inner = cs.inner.lock().unwrap();
        let lo = now_ms() - minutes * 60_000;
        struct Roll {
            b: f64,
            s: f64,
            n: u32,
        }
        let mut rolls: HashMap<String, Roll> = HashMap::new();
        let mut total = 0.0;
        for tr in inner.trades.iter().filter(|t| t.t >= lo) {
            let buy = tr.side.is_buy();
            let r = rolls.entry(tr.taker.clone()).or_insert(Roll { b: 0.0, s: 0.0, n: 0 });
            if buy {
                r.b += tr.usd;
            } else {
                r.s += tr.usd;
            }
            r.n += 1;
            if !tr.maker.is_empty() {
                let m = rolls.entry(tr.maker.clone()).or_insert(Roll { b: 0.0, s: 0.0, n: 0 });
                if buy {
                    m.s += tr.usd;
                } else {
                    m.b += tr.usd;
                }
            }
            total += tr.usd;
        }
        let mut rows: Vec<TraderRow> = rolls
            .into_iter()
            .map(|(a, r)| {
                let snap = self.trader_snapshot(&a);
                TraderRow {
                    a: a.clone(),
                    label: snap.as_ref().and_then(|s| s.label.clone()),
                    style: snap.as_ref().map(|s| s.style.clone()).unwrap_or_else(|| "NEW".into()),
                    tag: snap.as_ref().map(|s| s.tag.clone()).unwrap_or_else(|| "±".into()),
                    pnl: snap.as_ref().and_then(|s| s.pnl),
                    account_value: snap.as_ref().and_then(|s| s.account_value),
                    win_rate: snap.as_ref().and_then(|s| s.win_rate),
                    bought: r.b,
                    sold: r.s,
                    trades: r.n,
                    share: if total > 0.0 { (r.b + r.s) / total } else { 0.0 },
                    maker_ratio: None,
                    first_seen: snap.as_ref().map(|s| s.first_seen).unwrap_or(0),
                    last_seen: 0,
                    liq_count: snap.as_ref().map(|s| s.liq_count).unwrap_or(0),
                }
            })
            .collect();
        rows.sort_by(|a, b| (b.bought + b.sold).partial_cmp(&(a.bought + a.sold)).unwrap_or(std::cmp::Ordering::Equal));
        rows.truncate(60);
        rows
    }
}
