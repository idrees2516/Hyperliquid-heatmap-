//! Background tasks: asset context polling + wallet enrichment.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::engine::{now_ms, AppState};

/// Poll `metaAndAssetCtxs` and broadcast the asset universe.
/// Falls back to a static major-set if the endpoint stays unavailable
/// (shared-IP rate limits), so the UI dropdown still works.
pub async fn run_assets_poll(state: Arc<AppState>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(30));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut fails = 0u32;
    loop {
        ticker.tick().await;
        match state.rest.meta_and_asset_ctxs().await {
            Ok((universe, ctxs)) => {
                fails = 0;
                let mut assets: Vec<shared::AssetInfo> = universe
                    .iter()
                    .enumerate()
                    .filter_map(|(i, m)| {
                        let c = ctxs.get(i)?;
                        Some(shared::AssetInfo {
                            name: m.name.clone(),
                            mark: c.mark_px.parse().unwrap_or(0.0),
                            prev_day: c.prev_day_px.parse().unwrap_or(0.0),
                            day_ntl_vlm: c.day_ntl_vlm.parse().unwrap_or(0.0),
                            funding: c.funding.parse().unwrap_or(0.0),
                            open_interest: c.open_interest.parse().unwrap_or(0.0),
                            sz_decimals: m.sz_decimals,
                            max_leverage: m.max_leverage,
                        })
                    })
                    .collect();
                assets.retain(|a| a.mark > 0.0 && a.day_ntl_vlm > 10_000.0);
                assets.sort_by(|a, b| b.day_ntl_vlm.partial_cmp(&a.day_ntl_vlm).unwrap_or(std::cmp::Ordering::Equal));
                tracing::debug!(n = assets.len(), "assets refreshed");
                let _ = state
                    .broadcast
                    .send(crate::engine::EngineEvent::Assets(assets.clone()));
                *state.assets.write().unwrap() = assets;
            }
            Err(e) => {
                fails += 1;
                tracing::warn!("assets poll failed ({fails}): {e}");
                let still_empty = state.assets.read().unwrap().is_empty();
                if fails >= 3 && still_empty {
                    let fb: Vec<shared::AssetInfo> = crate::engine::FALLBACK_COINS
                        .iter()
                        .map(|n| shared::AssetInfo {
                            name: n.to_string(),
                            mark: 0.0,
                            prev_day: 0.0,
                            day_ntl_vlm: 0.0,
                            funding: 0.0,
                            open_interest: 0.0,
                            sz_decimals: 5,
                            max_leverage: 0,
                        })
                        .collect();
                    let _ = state
                        .broadcast
                        .send(crate::engine::EngineEvent::Assets(fb.clone()));
                    *state.assets.write().unwrap() = fb;
                    tracing::warn!("assets REST unavailable, using fallback market set");
                }
            }
        }
    }
}

/// Enrich the most active wallets with userFills (pnl / win rate / style)
/// and clearinghouseState (account value) for the top few.
pub async fn run_enrichment(state: Arc<AppState>) {
    let mut round = 0u32;
    let mut ticker = tokio::time::interval(Duration::from_secs(25));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        round += 1;

        // pick candidates: most active wallets seen recently, least-recently enriched
        let mut candidates: Vec<(String, i64, f64)> = {
            let traders = state.traders.read().unwrap();
            let now = now_ms();
            traders
                .iter()
                .map(|(a, t)| {
                    let g = t.lock().unwrap();
                    let age = g
                        .enriched
                        .as_ref()
                        .map(|e| now - e.updated)
                        .unwrap_or(i64::MAX);
                    (a.clone(), age, g.session_rpn)
                })
                .filter(|(a, _, _)| !a.is_empty())
                .collect::<Vec<_>>()
        };
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        // also sort ties by recent activity: keep it simple, age only
        let take: Vec<String> = candidates.iter().take(24).map(|(a, _, _)| a.clone()).collect();
        if take.is_empty() {
            continue;
        }

        let do_chs = round % 3 == 0; // every ~75s
        let chs_n = 8.min(take.len());

        for (i, addr) in take.iter().enumerate() {
            match state.rest.user_fills(addr).await {
                Ok(fills) => {
                    let av = if do_chs && i < chs_n {
                        state
                            .rest
                            .clearinghouse_state(addr)
                            .await
                            .ok()
                            .and_then(|v| {
                                v.first().map(|s| s.margin_summary.account_value.parse().unwrap_or(0.0))
                            })
                            .filter(|x| *x > 0.0)
                    } else {
                        None
                    };
                    let now = now_ms();
                    if let Some(enr) = crate::engine::traders::compute_enriched(&fills, av, now) {
                        if let Some(t) = state.traders.read().unwrap().get(addr) {
                            let t = t.clone();
                            let mut g = t.lock().unwrap();
                            let positions = enr.positions.clone();
                            let liq_fills = enr.liq_fills;
                            // wallets whose recent fills include liquidation force-closes
                            g.liq_count = g.liq_count.max(liq_fills);
                            g.enriched = Some(enr);
                            // correct live position sizes with exact on-chain state
                            for (coin, sz) in positions {
                                let e = g.positions.entry(coin).or_default();
                                if (e.size - sz).abs() / sz.abs().max(1.0) > 0.25 {
                                    e.size = sz;
                                    if sz.abs() < 1e-12 {
                                        e.avg_px = 0.0;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("userFills {addr} failed: {e}");
                }
            }
            tokio::time::sleep(Duration::from_millis(350)).await;
        }
    }
}

/// Periodically broadcast per-coin trader leaderboards for coins clients watch.
pub async fn run_traders_push(state: Arc<AppState>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(8));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        // find coins with client subscriptions
        let coins: Vec<String> = {
            let cs = state.coins.read().unwrap();
            cs.iter()
                .filter(|(_, c)| c.inner.lock().unwrap().sub_refcount > 0)
                .map(|(k, _)| k.clone())
                .collect()
        };
        for coin in coins {
            let rows = state.build_traders(&coin, 60);
            let _ = state.broadcast.send(crate::engine::EngineEvent::Traders {
                coin,
                rows,
            });
        }
    }
}

/// Keep a small map of well-known labels (auto MM detection is behavioral).
pub fn known_labels() -> HashMap<String, String> {
    // Intentionally empty: labels are detected behaviorally (MM detection) or
    // can be added here by operators (address -> label).
    HashMap::new()
}
