//! Liquidation radar: magnet levels, cascades and overhang.
//!
//! The radar turns observable on-chain flow into a liquidation-heatmap view:
//!
//! * Every aggressive OPEN / INCREASE on a coin seeds "estimated forced-flow"
//!   at the liquidation prices that position would have at common leverage
//!   (2% / 4% / 10% / 20% adverse moves, weighted). These seed **armed
//!   magnet levels** — price levels that would attract forced closures.
//! * When price trades into an armed level it becomes **engaged**; if forced
//!   flow (confirmed liquidation events or strict behavioral LIQ tags)
//!   follows within `radar_confirm_ms`, the level is **swept** and the burst
//!   is aggregated into a cascade record. If nothing confirms, the level
//!   re-arms with its estimate halved (the book was less leveraged than
//!   assumed).
//! * `last_cascade` records and armed levels are persisted, so **overhang
//!   signals survive restarts**.

use std::collections::{HashMap, VecDeque};

use shared::{CascadeRecord, LevelState, OverhangInfo, RadarLevel, RadarSnapshot, TuningParams};

/// leverage ladder: (adverse move fraction, weight of notional assigned)
const LEV_LADDER: [(f64, f64); 4] = [(0.02, 0.20), (0.04, 0.20), (0.10, 0.35), (0.20, 0.25)];

pub const MAX_LEVELS: usize = 1_500;
pub const MAX_CASCADES: usize = 120;

#[derive(Default)]
struct Burst {
    t_start: i64,
    t_last: i64,
    usd: f64,
    fills: u32,
    wallets: std::collections::HashSet<String>,
    px_lo: f64,
    px_hi: f64,
    slip_bps: f64,
    levels: std::collections::BTreeSet<i64>,
    /// armed est consumed on the way (bucket prices)
    level_px: Vec<f64>,
}

#[derive(Default)]
pub struct RadarState {
    pub coin: String,
    /// bucket-width of the radar grid (absolute price units)
    pub bucket: f64,
    pub last_px: f64,
    /// armed / engaged / swept levels keyed by bucket index
    pub levels: HashMap<i64, RadarLevel>,
    /// active cascade bursts keyed by direction (true = longs being liquidated)
    bursts: HashMap<bool, Burst>,
    /// recently completed cascades (newest last)
    pub last_cascade: VecDeque<CascadeRecord>,
    /// finalized records awaiting pickup by the engine (global history)
    pub new_cascades: VecDeque<CascadeRecord>,
    /// (t_engaged, bucket) of levels awaiting forced-flow confirmation
    pending: Vec<(i64, i64)>,
    pub t_updated: i64,
    pub dirty: bool,
}

impl RadarState {
    pub fn new(coin: &str) -> Self {
        RadarState {
            coin: coin.to_string(),
            ..Default::default()
        }
    }

    fn bucket_of(&self, px: f64) -> i64 {
        (px / self.bucket).floor() as i64
    }

    fn ensure_bucket(&mut self, px: f64, p: &TuningParams) {
        let want = shared::nice_step(px * p.radar_bucket_bps / 10_000.0);
        if want > 0.0 && (self.bucket <= 0.0 || (want / self.bucket - 1.0).abs() > 0.5) {
            // grid width changed drastically (new price regime): reset grid,
            // keep cascades. Bucket width is intentionally sticky.
            self.bucket = want;
        }
    }

    /// Seed armed levels from an aggressive open/increase.
    /// `is_buy` = direction of the *position* being opened.
    pub fn on_open(&mut self, px: f64, usd: f64, is_buy: bool, t: i64, p: &TuningParams) {
        if px <= 0.0 || usd <= 0.0 {
            return;
        }
        self.ensure_bucket(px, p);
        if self.bucket <= 0.0 {
            return;
        }
        // longs seeded below (they liquidate when price falls),
        // shorts seeded above (they liquidate when price rises)
        for (m, w) in LEV_LADDER {
            let est_px = if is_buy { px * (1.0 - m) } else { px * (1.0 + m) };
            let idx = self.bucket_of(est_px);
            let center = (idx as f64 + 0.5) * self.bucket;
            let lv = self.levels.entry(idx).or_insert_with(|| RadarLevel {
                px: center,
                long: is_buy,
                est_usd: 0.0,
                peak_usd: 0.0,
                swept_usd: 0.0,
                state: LevelState::Armed,
                t_first: t,
                t_last: t,
                hits: 0,
            });
            lv.est_usd += usd * w;
            lv.peak_usd = lv.peak_usd.max(lv.est_usd);
            lv.t_last = t;
            if lv.state == LevelState::Swept {
                // re-arming a swept level: new positions stacked on the zone
                lv.state = LevelState::Armed;
            }
        }
        if self.levels.len() > MAX_LEVELS {
            self.prune_levels(t);
        }
        self.t_updated = t;
    }

    /// Price tick: engage armed levels that price trades into.
    pub fn on_price(&mut self, px: f64, t: i64, p: &TuningParams) {
        if px <= 0.0 {
            return;
        }
        self.last_px = px;
        self.ensure_bucket(px, p);
        if self.bucket <= 0.0 {
            return;
        }
        let idx = self.bucket_of(px);
        let touched: Vec<i64> = self
            .levels
            .iter()
            .filter(|(k, lv)| {
                lv.state == LevelState::Armed
                    && ((lv.long && **k <= idx) || (!lv.long && **k >= idx))
            })
            .map(|(k, _)| *k)
            .take(64)
            .collect();
        for k in touched {
            if let Some(lv) = self.levels.get_mut(&k) {
                lv.state = LevelState::Engaged;
                lv.hits += 1;
                lv.t_last = t;
                self.pending.push((t, k));
            }
        }
        self.resolve_pending(t, p);
        self.finalize_bursts(t, p, false);
        self.t_updated = t;
    }

    /// Decay engaged levels that got no forced-flow confirmation in time.
    fn resolve_pending(&mut self, t: i64, p: &TuningParams) {
        let mut keep = Vec::new();
        for (t0, k) in self.pending.drain(..) {
            if t - t0 < p.radar_confirm_ms {
                keep.push((t0, k));
                continue;
            }
            if let Some(lv) = self.levels.get_mut(&k) {
                if lv.state == LevelState::Engaged {
                    // no forced flow followed: book was less leveraged than
                    // estimated — halve the remaining estimate and re-arm.
                    lv.est_usd *= 0.5;
                    if lv.est_usd < p.radar_level_usd * 0.25 {
                        lv.state = LevelState::Swept; // effectively nothing left
                    } else {
                        lv.state = LevelState::Armed;
                    }
                    lv.t_last = t;
                }
            }
        }
        self.pending = keep;
    }

    /// Feed a confirmed / strict-behavioral liquidation fill into the radar.
    pub fn on_liq_fill(
        &mut self,
        px: f64,
        usd: f64,
        long: bool,
        user: &str,
        slip_bps: f64,
        t: i64,
        p: &TuningParams,
    ) {
        if px <= 0.0 || usd <= 0.0 {
            return;
        }
        self.ensure_bucket(px, p);
        if self.bucket <= 0.0 {
            return;
        }
        // consume nearby engaged/armed levels on this side
        let idx = self.bucket_of(px);
        let reach = 8i64;
        let mut consumed_px: Vec<f64> = Vec::new();
        for d in -reach..=reach {
            let k = idx + d;
            let hit = {
                let Some(lv) = self.levels.get(&k) else { continue };
                lv.long == long
                    && (lv.state == LevelState::Armed || lv.state == LevelState::Engaged)
                    && (lv.px - px).abs() <= self.bucket * 12.0
            };
            if hit {
                let lv = self.levels.get_mut(&k).unwrap();
                lv.state = LevelState::Swept;
                lv.swept_usd += usd / 10.0; // spread attribution across levels
                lv.est_usd = (lv.est_usd - usd / 10.0).max(0.0);
                lv.t_last = t;
                consumed_px.push(lv.px);
            }
        }
        // aggregate into the directional burst
        let b = self
            .bursts
            .entry(long)
            .or_insert_with(Burst::default);
        let fresh = b.t_last == 0 || t - b.t_last <= p.cascade_burst_ms;
        if !fresh {
            self.finalize_bursts(t, p, false);
            let b = self.bursts.entry(long).or_default();
            b.t_start = t;
            b.usd = 0.0;
            b.fills = 0;
            b.wallets.clear();
            b.px_lo = 0.0;
            b.px_hi = 0.0;
            b.slip_bps = 0.0;
            b.levels.clear();
            b.level_px.clear();
        }
        // pre-compute bucket indices before the mutable burst borrow
        let bucket_w = self.bucket.max(1e-12);
        let b = self.bursts.get_mut(&long).unwrap();
        if b.t_start == 0 {
            b.t_start = t;
        }
        b.t_last = t;
        b.usd += usd;
        b.fills += 1;
        b.wallets.insert(user.to_string());
        b.px_lo = if b.px_lo == 0.0 { px } else { b.px_lo.min(px) };
        b.px_hi = b.px_hi.max(px);
        b.slip_bps = b.slip_bps.max(slip_bps);
        for cpx in consumed_px {
            let k = (cpx / bucket_w).floor() as i64;
            if b.levels.insert(k) {
                b.level_px.push(cpx);
            }
        }
        self.dirty = true;
        self.t_updated = t;
    }

    /// Finalize bursts that have gone quiet (or exceeded the max duration).
    fn finalize_bursts(&mut self, t: i64, p: &TuningParams, force: bool) {
        let keys: Vec<bool> = self.bursts.keys().copied().collect();
        for long in keys {
            let Some(b) = self.bursts.get(&long) else { continue };
            let expired = t - b.t_last > p.cascade_burst_ms
                || (t - b.t_start > p.cascade_max_ms && b.t_start > 0);
            if !expired && !force {
                continue;
            }
            let b = self.bursts.remove(&long).unwrap();
            if b.usd < p.cascade_min_usd {
                continue; // noise, not a cascade
            }
            let rec = CascadeRecord {
                coin: self.coin.clone(),
                t_start: b.t_start,
                t_end: b.t_last,
                long,
                usd: b.usd,
                fills: b.fills,
                wallets: b.wallets.len() as u32,
                px_lo: b.px_lo,
                px_hi: b.px_hi,
                slip_bps: b.slip_bps,
                levels: b.level_px.clone(),
                live: false,
            };
            self.last_cascade.push_back(rec.clone());
            while self.last_cascade.len() > MAX_CASCADES {
                self.last_cascade.pop_front();
            }
            self.new_cascades.push_back(rec);
            self.dirty = true;
        }
    }

    /// Finalize every open burst immediately (graceful shutdown).
    pub fn finalize_all(&mut self) {
        let t = self.t_updated.max(crate::engine::now_ms());
        let p = shared::TuningParams::default();
        self.finalize_bursts(t, &p, true);
    }

    /// Mark a level as swept with observed forced flow (confirmed events).
    pub fn sweep_at(&mut self, px: f64, usd: f64, long: bool, t: i64, p: &TuningParams) {
        self.ensure_bucket(px, p);
        if self.bucket <= 0.0 {
            return;
        }
        let idx = self.bucket_of(px);
        if let Some(lv) = self.levels.get_mut(&idx) {
            if lv.long == long {
                lv.state = LevelState::Swept;
                lv.swept_usd += usd;
                lv.est_usd = (lv.est_usd - usd).max(0.0);
                lv.t_last = t;
            }
        }
    }

    fn prune_levels(&mut self, t: i64) {
        // drop weakest + stalest levels when the grid overflows
        let mut all: Vec<(i64, f64, i64)> = self
            .levels
            .iter()
            .filter(|(_, lv)| lv.state == LevelState::Armed)
            .map(|(k, lv)| (*k, lv.est_usd, lv.t_last))
            .collect();
        all.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let keep: std::collections::HashSet<i64> = all
            .into_iter()
            .take(MAX_LEVELS / 2)
            .map(|(k, _, _)| k)
            .collect();
        self.levels
            .retain(|k, lv| keep.contains(k) || lv.state != LevelState::Armed);
        let _ = t;
    }

    /// Age-decay armed estimates (call every ~30s).
    pub fn decay(&mut self, t: i64, p: &TuningParams) {
        let half_ms = p.radar_decay_min * 60_000.0;
        for lv in self.levels.values_mut() {
            let age = (t - lv.t_last) as f64;
            if age > 0.0 && half_ms > 0.0 {
                let f = 0.5f64.powf(age / half_ms);
                if f < 0.999 {
                    lv.est_usd *= f;
                    self.dirty = true;
                }
            }
        }
        self.levels
            .retain(|_, lv| lv.est_usd >= 500.0 || lv.state != LevelState::Armed);
        self.finalize_bursts(t, p, false);
    }

    /// Build the wire snapshot (top levels by est_usd / recency + overhang).
    pub fn snapshot(&self, p: &TuningParams, now: i64) -> RadarSnapshot {
        let px = self.last_px;
        let mut levels: Vec<RadarLevel> = self
            .levels
            .values()
            .filter(|lv| {
                lv.state == LevelState::Swept
                    || lv.est_usd >= p.radar_level_usd * 0.25
            })
            .cloned()
            .collect();
        levels.sort_by(|a, b| {
            b.est_usd
                .partial_cmp(&a.est_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        levels.truncate(120);

        let mut oh = OverhangInfo {
            px,
            ..Default::default()
        };
        if px > 0.0 {
            for lv in self.levels.values() {
                if lv.state != LevelState::Armed || lv.est_usd < p.radar_level_usd {
                    continue;
                }
                let dist_pct = (lv.px - px).abs() / px * 100.0;
                if lv.long && lv.px <= px {
                    oh.long_usd_5 += if dist_pct <= 5.0 { lv.est_usd } else { 0.0 };
                    if oh.long_nearest_px.map(|c| lv.px > c).unwrap_or(true) {
                        oh.long_nearest_px = Some(lv.px);
                        oh.long_nearest_usd = lv.est_usd;
                    }
                } else if !lv.long && lv.px >= px {
                    oh.short_usd_5 += if dist_pct <= 5.0 { lv.est_usd } else { 0.0 };
                    if oh.short_nearest_px.map(|c| lv.px < c).unwrap_or(true) {
                        oh.short_nearest_px = Some(lv.px);
                        oh.short_nearest_usd = lv.est_usd;
                    }
                }
            }
        }
        let mut cascades: Vec<CascadeRecord> = self
            .last_cascade
            .iter()
            .rev()
            .take(30)
            .cloned()
            .collect();
        cascades.extend(self.bursts.iter().filter_map(|(long, b)| {
            if b.usd >= p.cascade_min_usd * 0.5 {
                Some(CascadeRecord {
                    coin: self.coin.clone(),
                    t_start: b.t_start,
                    t_end: b.t_last,
                    long: *long,
                    usd: b.usd,
                    fills: b.fills,
                    wallets: b.wallets.len() as u32,
                    px_lo: b.px_lo,
                    px_hi: b.px_hi,
                    slip_bps: b.slip_bps,
                    levels: b.level_px.clone(),
                    live: true,
                })
            } else {
                None
            }
        }));
        cascades.sort_by(|a, b| b.t_end.cmp(&a.t_end));

        RadarSnapshot {
            coin: self.coin.clone(),
            px,
            bucket: self.bucket,
            levels,
            overhang: oh,
            last_cascade: cascades,
            updated_at: now,
        }
    }

    /// Serialize for persistence.
    pub fn to_persist(&self) -> PersistRadar {
        PersistRadar {
            coin: self.coin.clone(),
            bucket: self.bucket,
            last_px: self.last_px,
            levels: self.levels.values().cloned().collect(),
            last_cascade: self.last_cascade.iter().cloned().collect(),
            t_updated: self.t_updated,
        }
    }

    pub fn from_persist(p: PersistRadar) -> Self {
        let mut s = RadarState::new(&p.coin);
        s.bucket = p.bucket;
        s.last_px = p.last_px;
        s.t_updated = p.t_updated;
        for lv in p.levels {
            s.levels.insert((lv.px / p.bucket.max(1e-12)).floor() as i64, lv);
        }
        for c in p.last_cascade {
            s.last_cascade.push_back(c);
        }
        s
    }
}

/// Persistence shape for radar.json.
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct PersistRadar {
    pub coin: String,
    pub bucket: f64,
    pub last_px: f64,
    pub levels: Vec<RadarLevel>,
    pub last_cascade: Vec<CascadeRecord>,
    pub t_updated: i64,
}
