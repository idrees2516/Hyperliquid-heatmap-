//! Engine: central application state + trade/liquidation/candle processing.

pub mod market;
pub mod tasks;
pub mod traders;

use std::collections::HashMap;
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
/// so the realtime pipeline always has flow.
pub const FALLBACK_COINS: &[&str] = &["BTC", "ETH", "SOL", "HYPE", "XRP", "DOGE"];

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
    Status { upstream: bool },
}

pub struct AppState {
    pub assets: RwLock<Vec<AssetInfo>>,
    pub coins: RwLock<HashMap<String, Arc<market::CoinState>>>,
    pub traders: RwLock<HashMap<String, Arc<Mutex<traders::TraderState>>>>,
    pub broadcast: broadcast::Sender<EngineEvent>,
    pub sub_cmds: mpsc::UnboundedSender<SubCmd>,
    pub rest: crate::hl::rest::HlRest,
    pub started_at: i64,
    pub upstream_alive: RwLock<bool>,
    pub default_coins: RwLock<Vec<String>>,
    /// pending liquidations awaiting their matching trade
    pending_liq: Mutex<HashMap<(String, String), (i64, bool)>>,
}

impl AppState {
    pub fn new(rest: crate::hl::rest::HlRest, sub_cmds: mpsc::UnboundedSender<SubCmd>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(8192);
        Arc::new(Self {
            assets: RwLock::new(Vec::new()),
            coins: RwLock::new(HashMap::new()),
            traders: RwLock::new(HashMap::new()),
            broadcast: tx,
            sub_cmds,
            rest,
            started_at: now_ms(),
            upstream_alive: RwLock::new(false),
            default_coins: RwLock::new(Vec::new()),
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

        // 1. position / intent for taker
        let (action, rpn, pending, hedge, pos_before_abs, pos_after_abs) = {
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
            g.push_recent(now, &raw.coin, side.is_buy(), usd);
            (action, rpn, pending, hedge, pos_before_abs, pos_after_abs)
        };

        let mut intent = traders::classify_intent(action, rpn, usd, pending, side);
        if hedge && matches!(intent, Intent::Aggressive) {
            intent = Intent::Hedge;
        }

        // behavioral liquidation: aggressive full close of a losing position.
        // The liquidation engine force-closes the whole position; a voluntary
        // stop usually leaves remainder or was placed earlier.
        let mut liq_forced = false;
        if !pending
            && matches!(action, shared::Action::Reduce | shared::Action::Flip)
            && rpn < 0.0
            && pos_before_abs > 0.0
            && pos_after_abs <= pos_before_abs * 0.05
            && pos_before_abs * px >= 1_500.0
        {
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

        // 3. behavioral liquidation event (full close of a losing position)
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

        // 4. broadcast
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

    // -----------------------------------------------------------------------
    // Liquidations
    // -----------------------------------------------------------------------

    pub fn process_liquidation(&self, user: &str, liq: &HlLiqInner, t: i64) {
        let szi: f64 = liq.szi.parse().unwrap_or(0.0);
        let px: f64 = liq.px.parse().unwrap_or(0.0);
        let usd = szi.abs() * px;
        // side in liquidation event: direction of the position that got liquidated
        // B = long position liquidated, A = short position liquidated
        let long = matches!(liq.side.as_str(), "B" | "b" | "long" | "Buy" | "buy");
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
            if g.label.is_none() {
                // liquidated wallets are notable
            }
        }
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
