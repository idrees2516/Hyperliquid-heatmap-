//! Live-tunable engine parameters (the TUNING tab).
//!
//! All intent-precision thresholds (stricter LIQ detection), radar/magnet
//! sizing and opportunity thresholds live here. Values can be changed at
//! runtime via `POST /api/tuning`, broadcast to every browser, and are
//! persisted to `<data>/tuning.json` so they survive restarts.

use std::path::PathBuf;
use std::sync::RwLock;

use shared::TuningParams;

pub struct Tuning {
    params: RwLock<TuningParams>,
    file: Option<PathBuf>,
    dirty: RwLock<bool>,
}

impl Tuning {
    pub fn load_or_default(data_dir: &std::path::Path) -> Self {
        let file = data_dir.join("tuning.json");
        let params = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str::<TuningParams>(&s).ok())
            .map(|p| p.clamped())
            .unwrap_or_default();
        if std::fs::read_to_string(&file).is_err() {
            // first boot: write defaults
            if let Ok(s) = serde_json::to_string_pretty(&params) {
                let _ = std::fs::write(&file, s);
            }
        }
        Self {
            params: RwLock::new(params),
            file: Some(file),
            dirty: RwLock::new(false),
        }
    }

    pub fn get(&self) -> TuningParams {
        self.params.read().unwrap().clone()
    }

    /// Apply a partial update (any subset of fields) and return the new set.
    pub fn update(&self, patch: &serde_json::Value) -> TuningParams {
        let mut p = self.params.write().unwrap();
        let base = serde_json::to_value(&*p).unwrap_or(serde_json::Value::Null);
        let merged = merge_json(base, patch);
        if let Ok(newp) = serde_json::from_value::<TuningParams>(merged) {
            *p = newp.clamped();
        }
        *self.dirty.write().unwrap() = true;
        p.clone()
    }

    pub fn reset(&self) -> TuningParams {
        let mut p = self.params.write().unwrap();
        *p = TuningParams::default();
        *self.dirty.write().unwrap() = true;
        p.clone()
    }

    pub fn save_if_dirty(&self) {
        if !*self.dirty.read().unwrap() {
            return;
        }
        let p = self.params.read().unwrap().clone();
        if let Some(f) = &self.file {
            if let Ok(s) = serde_json::to_string_pretty(&p) {
                if std::fs::write(f, s).is_ok() {
                    *self.dirty.write().unwrap() = false;
                    tracing::info!("tuning params persisted");
                }
            }
        }
    }
}

/// Shallow-merge a JSON object patch into a base object.
fn merge_json(base: serde_json::Value, patch: &serde_json::Value) -> serde_json::Value {
    match (base, patch) {
        (serde_json::Value::Object(mut b), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                b.insert(k.clone(), v.clone());
            }
            serde_json::Value::Object(b)
        }
        (_, p) => p.clone(),
    }
}

// ---------------------------------------------------------------------------
// Stricter liquidation intent: confidence scoring
// ---------------------------------------------------------------------------

/// Inputs to the behavioral liquidation detector for one candidate burst.
#[derive(Clone, Copy, Debug, Default)]
pub struct LiqSignal {
    /// fraction of the pre-trade position this burst closed (0..1)
    pub close_frac: f64,
    /// realized loss depth in bps of fill notional
    pub loss_bps: f64,
    /// aggressive same-direction fills by the same wallet in the last ~3s
    pub burst: u32,
    /// sweep slippage vs the price ~30s ago, in bps (always >= 0)
    pub slip_bps: f64,
    /// fill notional (USD)
    pub usd: f64,
}

/// Score a behavioral liquidation candidate in [0, 1].
///
/// A genuine forced close looks like: (a) the whole position goes at once,
/// (b) it realizes a real loss, (c) it prints as a rapid burst of aggressive
/// fills, (d) those fills sweep the book (slippage vs the recent mark).
pub fn liq_confidence(s: &LiqSignal) -> f64 {
    let close = (s.close_frac / 0.95).min(1.0);
    let loss = (s.loss_bps / 50.0).min(1.0);
    let burst = (s.burst.saturating_sub(1) as f64 / 5.0).min(1.0);
    let slip = (s.slip_bps / 25.0).min(1.0);
    (0.30 * close + 0.25 * loss + 0.20 * burst + 0.25 * slip).clamp(0.0, 1.0)
}

/// Verdict of the strict gate: is this burst a liquidation?
#[derive(Clone, Copy, Debug)]
pub struct LiqVerdict {
    pub is_liq: bool,
    pub confidence: f64,
    pub slip_bps: f64,
}

/// The strict gate: confidence must clear `liq_conf_cap` AND the fills must
/// have swept at least `liq_min_sweep_slip_bps` of slippage. A confirmed
/// liquidation event (userEvents) bypasses the gate entirely.
pub fn liq_verdict(s: &LiqSignal, p: &TuningParams, confirmed: bool) -> LiqVerdict {
    let confidence = if confirmed {
        1.0
    } else {
        liq_confidence(s)
    };
    let hard_gates = s.usd >= p.liq_min_usd
        && s.close_frac >= p.liq_min_close_frac
        && s.loss_bps >= p.liq_min_loss_bps;
    let is_liq = confirmed
        || (hard_gates
            && confidence >= p.liq_conf_cap
            && s.slip_bps >= p.liq_min_sweep_slip_bps);
    LiqVerdict {
        is_liq,
        confidence,
        slip_bps: s.slip_bps,
    }
}
