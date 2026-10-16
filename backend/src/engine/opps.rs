//! Opportunity engine: scans the live footprint, radar and trader registry to
//! surface actionable setups arising from detected events — liquidation
//! cascades, magnet levels, stop runs, absorption and whale prints.

use std::collections::HashMap;

use shared::{OppKind, Opportunity};

use crate::engine::{now_ms, AppState};

const SCAN_WINDOW_MS: i64 = 15 * 60_000;
const WHALE_WINDOW_MS: i64 = 6 * 60_000;

struct CoinScan {
    coin: String,
    px: f64,
    buy: f64,
    sell: f64,
    stop_buckets: Vec<(f64, f64)>, // (px, usd)
    whales: Vec<(bool, f64, i64, String)>, // (is_buy, usd, t, addr)
    has_flow: bool,
}

pub fn build_opps(state: &AppState) -> Vec<Opportunity> {
    let p = state.tuning.get();
    let now = now_ms();
    let mut out: Vec<Opportunity> = Vec::new();

    // ---- collect candidate coins with recent flow ------------------------
    let mut scans: Vec<CoinScan> = Vec::new();
    {
        let coins = state.coins.read().unwrap();
        for (name, cs) in coins.iter() {
            let inner = cs.inner.lock().unwrap();
            let last_t = inner.trades.back().map(|t| t.t).unwrap_or(0);
            if now - last_t > SCAN_WINDOW_MS {
                continue;
            }
            let lo = now - SCAN_WINDOW_MS;
            let (mut buy, mut sell) = (0.0, 0.0);
            let mut total = 0.0;
            let mut stop_b: HashMap<i64, (f64, f64)> = HashMap::new(); // idx -> (center, usd)
            let mut whales: Vec<(bool, f64, i64, String)> = Vec::new();
            let bucket = if inner.tick_est > 0.0 {
                shared::nice_step(inner.last_px * p.radar_bucket_bps / 10_000.0).max(inner.tick_est)
            } else if inner.last_px > 0.0 {
                shared::nice_step(inner.last_px * p.radar_bucket_bps / 10_000.0)
            } else {
                continue;
            };
            if bucket <= 0.0 || !bucket.is_finite() {
                continue;
            }
            for tr in inner.trades.iter().filter(|t| t.t >= lo) {
                total += tr.usd;
                if tr.side.is_buy() {
                    buy += tr.usd;
                } else {
                    sell += tr.usd;
                }
                if matches!(tr.intent, shared::Intent::StopLoss) && tr.usd > 0.0 {
                    let idx = (tr.px / bucket).floor() as i64;
                    let e = stop_b.entry(idx).or_insert(((idx as f64 + 0.5) * bucket, 0.0));
                    e.1 += tr.usd;
                }
                if tr.t >= now - WHALE_WINDOW_MS && tr.usd >= p.opp_whale_usd {
                    whales.push((tr.side.is_buy(), tr.usd, tr.t, tr.taker.clone()));
                }
            }
            if total < 250_000.0 {
                continue; // too thin to trade the setup
            }
            let mut stop_buckets: Vec<(f64, f64)> = stop_b.into_values().collect();
            stop_buckets.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            stop_buckets.truncate(6);
            scans.push(CoinScan {
                coin: name.clone(),
                px: inner.last_px,
                buy,
                sell,
                stop_buckets,
                whales,
                has_flow: total > 0.0,
            });
        }
    }
    scans.sort_by(|a, b| {
        (b.buy + b.sell)
            .partial_cmp(&(a.buy + a.sell))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scans.truncate(24);

    for sc in &scans {
        if !sc.has_flow || sc.px <= 0.0 {
            continue;
        }
        let radar = state.radar.read().unwrap().get(&sc.coin).cloned();
        let rsnap = radar.as_ref().map(|r| r.lock().unwrap().snapshot(&p, now));

        // ---- 1. magnet pull: armed overhang near price --------------------
        if let Some(rs) = &rsnap {
            let oh = &rs.overhang;
            if oh.px > 0.0 {
                if let Some(lp) = oh.long_nearest_px {
                    let dist = (sc.px - lp) / sc.px * 100.0;
                    if dist <= p.opp_magnet_dist_pct && oh.long_nearest_usd >= p.opp_magnet_usd {
                        let near = 1.0 - (dist / p.opp_magnet_dist_pct).clamp(0.0, 1.0);
                        let size = ((oh.long_nearest_usd / p.opp_magnet_usd).ln() / 3.0).clamp(0.0, 1.0);
                        out.push(Opportunity {
                            id: format!("magnet:{}:{lp:.4}", sc.coin),
                            kind: OppKind::MagnetPull,
                            coin: sc.coin.clone(),
                            title: format!("downside magnet {:.2}% away", dist),
                            detail: format!(
                                "{} armed long-liq flow at {} — price tends to be pulled into forced-flow pockets; expect a liquidity grab toward the cluster",
                                shared::fmt_usd(oh.long_nearest_usd),
                                lp,
                            ),
                            strength: 35.0 + 35.0 * near + 30.0 * size,
                            t: now,
                            ref_px: sc.px,
                            target_px: Some(lp),
                            side_bias: Some(false),
                            ttl_ms: 30 * 60_000,
                        });
                    }
                }
                if let Some(sp) = oh.short_nearest_px {
                    let dist = (sp - sc.px) / sc.px * 100.0;
                    if dist <= p.opp_magnet_dist_pct && oh.short_nearest_usd >= p.opp_magnet_usd {
                        let near = 1.0 - (dist / p.opp_magnet_dist_pct).clamp(0.0, 1.0);
                        let size = ((oh.short_nearest_usd / p.opp_magnet_usd).ln() / 3.0).clamp(0.0, 1.0);
                        out.push(Opportunity {
                            id: format!("magnet:{}:{sp:.4}", sc.coin),
                            kind: OppKind::MagnetPull,
                            coin: sc.coin.clone(),
                            title: format!("upside magnet {:.2}% away", dist),
                            detail: format!(
                                "{} armed short-liq flow at {} — shorts' forced covers sit above; a push into the cluster can accelerate",
                                shared::fmt_usd(oh.short_nearest_usd),
                                sp,
                            ),
                            strength: 35.0 + 35.0 * near + 30.0 * size,
                            t: now,
                            ref_px: sc.px,
                            target_px: Some(sp),
                            side_bias: Some(true),
                            ttl_ms: 30 * 60_000,
                        });
                    }
                }
            }

            // ---- 2. live sweep + 3. cascade exhaustion --------------------
            for c in &rs.last_cascade {
                if c.live && c.usd >= p.cascade_min_usd {
                    let age = (now - c.t_end).max(0);
                    let dir = if c.long { "longs" } else { "shorts" };
                    out.push(Opportunity {
                        id: format!("sweep:{}:{}", sc.coin, c.t_start),
                        kind: OppKind::SweepLive,
                        coin: sc.coin.clone(),
                        title: format!("{} being liquidated now", dir),
                        detail: format!(
                            "{} forced flow over {} fills / {} wallets in {}s — cascade in progress; trade with the forced direction, fade only after it exhausts",
                            shared::fmt_usd(c.usd),
                            c.fills,
                            c.wallets,
                            (c.t_end - c.t_start) / 1000,
                        ),
                        strength: 50.0 + 40.0 * ((c.usd / (p.cascade_min_usd * 10.0)).clamp(0.0, 1.0)) - 10.0 * (age as f64 / 30_000.0).min(1.0),
                        t: now,
                        ref_px: sc.px,
                        target_px: None,
                        side_bias: Some(!c.long), // forced selling = downside
                        ttl_ms: 2 * 60_000,
                    });
                } else if !c.live && now - c.t_end < 90_000 && c.usd >= p.cascade_min_usd * 2.0 {
                    let range = (c.px_hi - c.px_lo).max(1e-12);
                    let (retrace, dir) = if c.long {
                        ((sc.px - c.px_lo) / range, "long flush")
                    } else {
                        ((c.px_hi - sc.px) / range, "short squeeze")
                    };
                    if retrace >= 0.15 {
                        out.push(Opportunity {
                            id: format!("exhaust:{}:{}", sc.coin, c.t_end),
                            kind: OppKind::CascadeExhaustion,
                            coin: sc.coin.clone(),
                            title: format!("{} exhausted — {:.0}% retraced", dir, retrace * 100.0),
                            detail: format!(
                                "{} forced flow finished {}s ago and price has come {:.0}% of the cascade range back — forced sellers are done; mean-reversion window while the re-claim holds",
                                shared::fmt_usd(c.usd),
                                (now - c.t_end) / 1000,
                                retrace * 100.0,
                            ),
                            strength: 40.0 + 45.0 * retrace.min(1.0) + 15.0 * ((c.usd / (p.cascade_min_usd * 6.0)).clamp(0.0, 1.0)),
                            t: now,
                            ref_px: sc.px,
                            target_px: Some(if c.long { c.px_hi } else { c.px_lo }),
                            side_bias: Some(!c.long),
                            ttl_ms: 5 * 60_000,
                        });
                    }
                }

                // ---- 7. swept-level S/R on retest -------------------------
                if !c.live && now - c.t_end < 6 * 3600_000 {
                    for lp in &c.levels {
                        let dist_pct = (sc.px - *lp).abs() / sc.px * 100.0;
                        if dist_pct <= 0.35 && *lp > 0.0 {
                            let (bias, what) = if c.long && sc.px >= *lp {
                                (true, "support")
                            } else if !c.long && sc.px <= *lp {
                                (false, "resistance")
                            } else {
                                continue;
                            };
                            out.push(Opportunity {
                                id: format!("sr:{}:{lp:.4}", sc.coin),
                                kind: OppKind::SweptLevel,
                                coin: sc.coin.clone(),
                                title: format!("retesting swept {} @ {}", what, lp),
                                detail: format!(
                                    "a {} cascade cleared this zone {} ago — the leverage that lived here is gone, so the level often holds on retest while it re-arms",
                                    shared::fmt_usd(c.usd),
                                    ms_ago(now - c.t_end),
                                ),
                                strength: 42.0,
                                t: now,
                                ref_px: sc.px,
                                target_px: Some(*lp),
                                side_bias: Some(bias),
                                ttl_ms: 60 * 60_000,
                            });
                            break;
                        }
                    }
                }
            }
        }

        // ---- 4. stop-run / stop magnets ----------------------------------
        for (sp, susd) in sc.stop_buckets.iter().take(3) {
            if *susd < p.opp_stop_usd {
                continue;
            }
            let dist = (sp - sc.px) / sc.px * 100.0;
            if dist.abs() <= 1.0 {
                // cluster within reach: magnet toward it
                let bias = *sp > sc.px;
                out.push(Opportunity {
                    id: format!("stopm:{}:{sp:.4}", sc.coin),
                    kind: OppKind::StopRun,
                    coin: sc.coin.clone(),
                    title: format!("stop cluster {} {:.2}% away", if bias { "above" } else { "below" }, dist.abs()),
                    detail: format!(
                        "{} of stop-loss flow printed at {} in the last 15m — resting stops are fuel; price is often drawn to sweep them before continuing",
                        shared::fmt_usd(*susd),
                        sp,
                    ),
                    strength: 38.0 + 30.0 * ((*susd / (p.opp_stop_usd * 3.0)).clamp(0.0, 1.0)) + 22.0 * (1.0 - dist.abs().min(1.0)),
                    t: now,
                    ref_px: sc.px,
                    target_px: Some(*sp),
                    side_bias: Some(bias),
                    ttl_ms: 10 * 60_000,
                });
            } else if dist.abs() <= 2.5 && now % 2 == 0 {
                // already run through within the window: continuation
                let ran_up = *sp < sc.px;
                out.push(Opportunity {
                    id: format!("stopr:{}:{sp:.4}", sc.coin),
                    kind: OppKind::StopRun,
                    coin: sc.coin.clone(),
                    title: format!("stops at {} already swept", sp),
                    detail: format!(
                        "{} of stops printed at {} and price has traded {} beyond — the path of least resistance stays with the run while flow stays one-sided",
                        shared::fmt_usd(*susd),
                        sp,
                        format!("{:.2}%", dist.abs()),
                    ),
                    strength: 40.0 + 25.0 * ((*susd / (p.opp_stop_usd * 3.0)).clamp(0.0, 1.0)),
                    t: now,
                    ref_px: sc.px,
                    target_px: None,
                    side_bias: Some(ran_up),
                    ttl_ms: 10 * 60_000,
                });
            }
        }

        // ---- 5. delta divergence / absorption -----------------------------
        {
            let total = sc.buy + sc.sell;
            if total >= 1_000_000.0 {
                let imbalance = (sc.buy - sc.sell) / total;
                // price move from the oldest candle in the window
                let px_move = coin_move_pct(state, &sc.coin, SCAN_WINDOW_MS, now, sc.px);
                if let Some(mv) = px_move {
                    if imbalance > 0.35 && mv < 0.03 {
                        out.push(Opportunity {
                            id: format!("absorb:{}:{}", sc.coin, "sell"),
                            kind: OppKind::DeltaDivergence,
                            coin: sc.coin.clone(),
                            title: "buyers absorbed at the ask".into(),
                            detail: format!(
                                "takers bought {} net ({:.0}% of flow) but price is flat — passive size is unloading into aggression; bearish while the absorption holds",
                                shared::fmt_usd(sc.buy - sc.sell),
                                (sc.buy / total * 100.0),
                            ),
                            strength: 45.0 + 40.0 * imbalance.min(1.0),
                            t: now,
                            ref_px: sc.px,
                            target_px: None,
                            side_bias: Some(false),
                            ttl_ms: 15 * 60_000,
                        });
                    } else if imbalance < -0.35 && mv > -0.03 {
                        out.push(Opportunity {
                            id: format!("absorb:{}:{}", sc.coin, "buy"),
                            kind: OppKind::DeltaDivergence,
                            coin: sc.coin.clone(),
                            title: "sellers absorbed at the bid".into(),
                            detail: format!(
                                "takers sold {} net ({:.0}% of flow) but price refuses to break down — passive bids are eating the aggression; bullish while the absorption holds",
                                shared::fmt_usd(sc.sell - sc.buy),
                                (sc.sell / total * 100.0),
                            ),
                            strength: 45.0 + 40.0 * (-imbalance).min(1.0),
                            t: now,
                            ref_px: sc.px,
                            target_px: None,
                            side_bias: Some(true),
                            ttl_ms: 15 * 60_000,
                        });
                    }
                }
            }
        }

        // ---- 6. whale prints ---------------------------------------------
        for (is_buy, usd, t, addr) in &sc.whales {
            let tag = state
                .trader_snapshot(addr)
                .map(|s| s.tag)
                .unwrap_or_else(|| "±".into());
            let win = matches!(tag.as_str(), "++" | "+");
            if !win {
                continue;
            }
            let age = (now - t).max(0);
            out.push(Opportunity {
                id: format!("whale:{}:{t}", sc.coin),
                kind: OppKind::WhalePrint,
                coin: sc.coin.clone(),
                title: format!("winning wallet {} {}", if *is_buy { "bought" } else { "sold" }, shared::fmt_usd(*usd)),
                detail: format!(
                    "a {} conviction wallet just took a {} print ({}) — historically green wallets pressing size are worth following on retests of their entry",
                    tag,
                    if *is_buy { "long" } else { "short" },
                    ms_ago(age),
                ),
                strength: 40.0 + 45.0 * ((*usd / (p.opp_whale_usd * 4.0)).clamp(0.0, 1.0)),
                t: *t,
                ref_px: sc.px,
                target_px: None,
                side_bias: Some(*is_buy),
                ttl_ms: 10 * 60_000,
            });
        }
    }

    out.retain(|o| o.strength > 0.0 && o.strength <= 100.0);
    out.sort_by(|a, b| b.strength.partial_cmp(&a.strength).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(48);
    out
}

/// % price move over the window from candles (fallback to tape).
fn coin_move_pct(state: &AppState, coin: &str, window_ms: i64, now: i64, last_px: f64) -> Option<f64> {
    let cs = state.get_coin(coin)?;
    let inner = cs.inner.lock().unwrap();
    let lo = now - window_ms;
    let mut first: Option<f64> = None;
    for (_, c) in inner.candles.range(lo..) {
        if first.is_none() {
            first = Some(c.o);
        }
    }
    let start = match first {
        Some(f) if f > 0.0 => f,
        _ => inner
            .trades
            .iter()
            .find(|t| t.t >= lo)
            .map(|t| t.px)
            .filter(|p| *p > 0.0)?,
    };
    let end = if last_px > 0.0 { last_px } else { inner.last_px };
    if start <= 0.0 || end <= 0.0 {
        return None;
    }
    Some((end / start - 1.0) * 100.0)
}

/// short humanized age ("4s ago", "2m ago", "1h ago")
fn ms_ago(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else {
        format!("{}h ago", s / 3600)
    }
}
