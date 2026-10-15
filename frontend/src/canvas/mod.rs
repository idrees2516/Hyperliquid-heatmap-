//! Canvas footprint chart renderer.
//!
//! Classic footprint (bid×ask / delta) layout: every visible time window
//! gets a slim candle on the left of its slot and per-price-level cells
//! [ SELL | BUY ] heat-mapped by USD notional, with the point-of-control
//! outlined and a per-level intent bar (LIQ/STOP/TP/HEDGE).
//! The current window is framed + tagged LIVE and updates in realtime.

use std::collections::BTreeMap;

use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

use crate::ui::{fmt_k, fmt_px, ms_hm};
use crate::AppState;

const SLOT_W: f64 = 68.0;
const AXIS_W: f64 = 60.0;
const DELTA_H: f64 = 44.0;
const TIME_H: f64 = 15.0;
const TOP_PAD: f64 = 22.0;

const C_BUY: &str = "#1fc77e";
const C_SELL: &str = "#f0435c";
const C_ACCENT: &str = "#97fce4";
const C_TXT: &str = "#dfe9f3";
const C_DIM: &str = "#54677d";
const C_GRID: &str = "#0d141d";
const C_LIQ: &str = "#f97316";
const C_STOP: &str = "#ef4444";
const C_TP: &str = "#22c55e";
const C_HEDGE: &str = "#a78bfa";

/// Geometry of the last draw; reused for hit-testing (tooltip / clicks).
#[derive(Clone)]
pub struct Slot {
    pub t: i64,
    pub x: f64,
    pub w: f64,
    pub last: bool,
    pub levels: Vec<f64>,
}

#[derive(Clone)]
pub struct Layout {
    pub plot_x: f64,
    pub plot_w: f64,
    pub plot_y: f64,
    pub plot_h: f64,
    pub price_lo: f64,
    pub price_hi: f64,
    pub slots: Vec<Slot>,
    pub bucket: f64,
}

pub struct Hit {
    pub t: i64,
    pub level: Option<f64>,
}

fn nice_step(raw: f64) -> f64 {
    if raw <= 0.0 || !raw.is_finite() {
        return 1.0;
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

/// Hit-test against the last drawn layout.
pub fn hit_test(x: f64, y: f64) -> Option<Hit> {
    LAYOUT.with(|lc| {
        let layout = lc.borrow();
        let layout = layout.as_ref()?;
        if y < layout.plot_y || y > layout.plot_y + layout.plot_h + DELTA_H {
            return None;
        }
        let slot = layout
            .slots
            .iter()
            .find(|s| x >= s.x - 2.0 && x <= s.x + s.w + 2.0)?;
        if y < layout.plot_y || y > layout.plot_y + layout.plot_h {
            return Some(Hit { t: slot.t, level: None });
        }
        // nearest level row within half row height
        let price_span = layout.price_hi - layout.price_lo;
        if price_span <= 0.0 {
            return Some(Hit { t: slot.t, level: None });
        }
        let p = layout.price_hi - (y - layout.plot_y) / layout.plot_h * price_span;
        let mut best: Option<(f64, f64)> = None; // (dist, level price)
        for lp in &slot.levels {
            let ly = layout.plot_y + (layout.price_hi - *lp) / price_span * layout.plot_h;
            let d = (ly - y).abs();
            if d <= 8.0 && best.map(|(bd, _)| d < bd).unwrap_or(true) {
                best = Some((d, *lp));
            }
        }
        Some(Hit {
            t: slot.t,
            level: best.map(|(_, lp)| lp),
        })
    })
}

pub fn draw(st: &AppState, canvas: &HtmlCanvasElement) {
    let ctx = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|c| c.dyn_into::<CanvasRenderingContext2d>().ok());
    let Some(ctx) = ctx else { return };

    let window = web_sys::window().expect("window");
    let dpr = window.device_pixel_ratio().max(1.0);
    let cw = canvas.client_width() as f64;
    let ch = canvas.client_height() as f64;
    if cw < 80.0 || ch < 120.0 {
        return;
    }
    let w_px = (cw * dpr) as u32;
    let h_px = (ch * dpr) as u32;
    if canvas.width() != w_px {
        canvas.set_width(w_px);
        canvas.set_height(h_px);
    }
    let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
    ctx.set_fill_style_str("#04070c");
    ctx.fill_rect(0.0, 0.0, cw, ch);

    // ----- merge windows + candle-only gaps into a display list -------------
    let mut merged: BTreeMap<i64, (Option<&shared::FootWindow>, Option<&shared::Candle>)> =
        BTreeMap::new();
    for w in &st.windows {
        merged.entry(w.t).or_insert((None, None)).0 = Some(w);
    }
    for (&t, c) in &st.candle_gaps {
        let e = merged.entry(t).or_insert((None, None));
        if e.0.is_none() {
            e.1 = Some(c);
        }
    }

    let plot_x = 8.0;
    let plot_w = (cw - AXIS_W - 12.0).max(100.0);
    let delta_y = ch - TIME_H - DELTA_H;
    let plot_y = TOP_PAD;
    let plot_h = (delta_y - plot_y - 6.0).max(60.0);

    if merged.is_empty() {
        ctx.set_fill_style_str(C_DIM);
        ctx.set_font("12px ui-monospace, monospace");
        ctx.set_text_align("center");
        let _ = ctx.fill_text("aggregating on-chain fills …", cw / 2.0, ch / 2.0);
        ctx.set_text_align("left");
        return;
    }

    let k = ((plot_w / SLOT_W) as usize).clamp(6, 60);
    let vis: Vec<(i64, Option<&shared::FootWindow>, Option<&shared::Candle>)> = {
        let mut all: Vec<(i64, Option<&shared::FootWindow>, Option<&shared::Candle>)> = merged
            .iter()
            .map(|(t, (w, c))| (*t, *w, *c))
            .collect();
        let n_vis = all.len().min(k);
        all.split_off(all.len() - n_vis)
    }; // last k windows, chronological
    let n = vis.len();

    // ----- price domain -----------------------------------------------------
    let (mut p_lo, mut p_hi) = (f64::MAX, f64::MIN);
    for (_, w, c) in &vis {
        let (l, h) = if let Some(w) = w {
            (w.l, w.h)
        } else if let Some(c) = c {
            (c.l, c.h)
        } else {
            continue;
        };
        if l > 0.0 {
            p_lo = p_lo.min(l);
        }
        p_hi = p_hi.max(h);
    }
    if p_lo == f64::MAX || p_hi <= 0.0 {
        p_lo = st.last_px * 0.999;
        p_hi = st.last_px * 1.001;
    }
    if p_hi - p_lo < 1e-9 {
        let pad = if st.tick > 0.0 { st.tick * 50.0 } else { p_hi * 0.0005 };
        p_lo -= pad;
        p_hi += pad;
    }
    let pad = (p_hi - p_lo) * 0.05;
    p_lo -= pad;
    p_hi += pad;

    let p2y = |p: f64| plot_y + (p_hi - p) / (p_hi - p_lo) * plot_h;

    // ----- horizontal grid + right axis ------------------------------------
    let step = nice_step((p_hi - p_lo) / (plot_h / 42.0).max(2.0));
    ctx.set_font("9px ui-monospace, monospace");
    ctx.set_text_baseline("middle");
    let mut gp = (p_lo / step).ceil() * step;
    while gp < p_hi {
        let y = p2y(gp);
        ctx.set_stroke_style_str(C_GRID);
        ctx.set_line_width(1.0);
        ctx.begin_path();
        ctx.move_to(plot_x, y);
        ctx.line_to(plot_x + plot_w, y);
        ctx.stroke();
        ctx.set_fill_style_str("#5b6d82");
        ctx.set_text_align("left");
        let _ = ctx.fill_text(&fmt_px(gp), plot_x + plot_w + 8.0, y);
        gp += step;
    }

    // ----- per-slot rendering ----------------------------------------------
    let mut slots: Vec<Slot> = Vec::with_capacity(n);
    let mut max_delta: f64 = 1.0;
    for (_, w, _) in &vis {
        if let Some(w) = w {
            max_delta = max_delta.max(w.delta.abs());
        }
    }
    let mut max_vol = 0.0f64;
    for (_, w, _) in &vis {
        if let Some(w) = w {
            for lv in &w.levels {
                max_vol = max_vol.max(lv.buy).max(lv.sell);
            }
        }
    }
    if max_vol <= 0.0 {
        max_vol = 1.0;
    }

    let delta_mid = delta_y + DELTA_H * 0.45;
    let delta_scale = (DELTA_H * 0.42 - 4.0).max(8.0);

    for (i, (t, w, c)) in vis.iter().enumerate() {
        let x = plot_x + (i as f64) * SLOT_W + 3.0;
        let w_slot = SLOT_W - 6.0;
        let is_last = i + 1 == n;
        let t = *t;
        // slot separator
        if i > 0 {
            ctx.set_stroke_style_str("#0c1320");
            ctx.begin_path();
            ctx.move_to(x - 3.5, plot_y - 4.0);
            ctx.line_to(x - 3.5, delta_y + DELTA_H);
            ctx.stroke();
        }

        let (o, h, l, c_price, has_flow, win) = if let Some(w) = w {
            (w.o, w.h, w.l, w.c, w.prints > 0, Some(w))
        } else if let Some(c) = c {
            (c.o, c.h, c.l, c.c, false, None)
        } else {
            continue;
        };

        // ---- candle (slim, left of the slot) ----
        let cx = x + 7.0;
        let up = c_price >= o;
        let col = if has_flow {
            if up { C_BUY } else { C_SELL }
        } else {
            "#2c3a4d" // candle-only gap: dim
        };
        ctx.set_stroke_style_str(col);
        ctx.set_line_width(1.0);
        ctx.begin_path();
        ctx.move_to(cx, p2y(h));
        ctx.line_to(cx, p2y(l));
        ctx.stroke();
        let yo = p2y(o);
        let yc = p2y(c_price);
        let (ty, bh) = if yc >= yo { (yo, yc - yo) } else { (yc, yo - yc) };
        ctx.set_fill_style_str(col);
        ctx.set_global_alpha(if has_flow { 0.95 } else { 0.55 });
        ctx.fill_rect(cx - 3.5, ty, 7.0, bh.max(1.0));
        ctx.set_global_alpha(1.0);

        // ---- footprint cells ----
        let mut level_ps: Vec<f64> = Vec::new();
        if let Some(w) = win {
            if w.prints > 0 && st.bucket > 0.0 {
                let cell_x0 = x + 16.0;
                let cell_x1 = x + w_slot - 1.0;
                let half = ((cell_x1 - cell_x0) / 2.0).max(12.0);
                let w_max = w
                    .levels
                    .iter()
                    .map(|lv| lv.buy.max(lv.sell))
                    .fold(0.0f64, f64::max)
                    .max(1e-9);

                for lv in &w.levels {
                    if lv.p < p_lo || lv.p > p_hi || lv.total() <= 0.0 {
                        continue;
                    }
                    let y = p2y(lv.p);
                    level_ps.push(lv.p);

                    // heat backgrounds
                    let sa = 0.05 + 0.50 * (lv.sell / w_max).min(1.0);
                    let ba = 0.05 + 0.50 * (lv.buy / w_max).min(1.0);
                    ctx.set_global_alpha(sa);
                    ctx.set_fill_style_str(C_SELL);
                    ctx.fill_rect(cell_x0, y - 6.5, half - 1.0, 13.0);
                    ctx.set_global_alpha(ba);
                    ctx.set_fill_style_str(C_BUY);
                    ctx.fill_rect(cell_x0 + half + 1.0, y - 6.5, half - 1.0, 13.0);
                    ctx.set_global_alpha(1.0);

                    // imbalance emphasis
                    let imb_buy = lv.buy > 2.2 * lv.sell && lv.buy > 0.0;
                    let imb_sell = lv.sell > 2.2 * lv.buy && lv.sell > 0.0;
                    ctx.set_font(if imb_buy || imb_sell {
                        "bold 8.5px ui-monospace, monospace"
                    } else {
                        "8.5px ui-monospace, monospace"
                    });
                    ctx.set_text_align("center");
                    ctx.set_text_baseline("middle");
                    ctx.set_fill_style_str(if imb_sell { "#ff8fa0" } else { "#9aa8ba" });
                    let _ = ctx.fill_text(&fmt_k(lv.sell), cell_x0 + (half - 1.0) / 2.0, y);
                    ctx.set_fill_style_str(if imb_buy { "#7ef0bb" } else { "#9aa8ba" });
                    let _ = ctx.fill_text(&fmt_k(lv.buy), cell_x0 + half + 1.0 + (half - 1.0) / 2.0, y);

                    // POC outline
                    if (lv.p - w.poc).abs() < st.bucket * 0.25 {
                        ctx.set_stroke_style_str(C_ACCENT);
                        ctx.set_line_width(1.0);
                        ctx.set_global_alpha(0.9);
                        ctx.stroke_rect(cell_x0 - 1.5, y - 7.5, (cell_x1 - cell_x0) + 2.0, 15.0);
                        ctx.set_global_alpha(1.0);
                    }

                    // intent bar under the cells
                    let tagged = lv.liq + lv.stop + lv.tp + lv.hedge;
                    if tagged > 0.0 {
                        let mut bx = cell_x0;
                        let seg = |ctx: &CanvasRenderingContext2d, from: f64, w: f64, c: &str| {
                            if w <= 0.05 {
                                return;
                            }
                            ctx.set_fill_style_str(c);
                            ctx.fill_rect(from, y + 8.0, w, 2.0);
                        };
                        let tot_w = cell_x1 - cell_x0;
                        seg(&ctx, bx, lv.liq / tagged * tot_w, C_LIQ);
                        bx += lv.liq / tagged * tot_w;
                        seg(&ctx, bx, lv.stop / tagged * tot_w, C_STOP);
                        bx += lv.stop / tagged * tot_w;
                        seg(&ctx, bx, lv.tp / tagged * tot_w, C_TP);
                        bx += lv.tp / tagged * tot_w;
                        seg(&ctx, bx, lv.hedge / tagged * tot_w, C_HEDGE);
                    }
                }
            }
        }

        // ---- delta strip ----
        if let Some(w) = win {
            let dh = w.delta / max_delta * delta_scale;
            let col = if w.delta >= 0.0 { C_BUY } else { C_SELL };
            ctx.set_fill_style_str(col);
            ctx.set_global_alpha(0.85);
            if dh >= 0.0 {
                ctx.fill_rect(x + 8.0, delta_mid - dh, w_slot - 16.0, dh.max(1.0));
            } else {
                ctx.fill_rect(x + 8.0, delta_mid, w_slot - 16.0, (-dh).max(1.0));
            }
            ctx.set_global_alpha(1.0);
        }

        // ---- time label ----
        let stride = (n / 7).max(1);
        if (n - 1 - i) % stride == 0 || is_last {
            ctx.set_fill_style_str("#5b6d82");
            ctx.set_font("8.5px ui-monospace, monospace");
            ctx.set_text_align("center");
            let _ = ctx.fill_text(&ms_hm(t), x + w_slot / 2.0, ch - TIME_H + 9.0);
        }

        // ---- LIVE frame on the current window ----
        if is_last {
            ctx.set_stroke_style_str(C_ACCENT);
            ctx.set_line_width(1.0);
            ctx.set_global_alpha(0.55);
            ctx.stroke_rect(x - 1.5, plot_y - 8.0, w_slot + 3.0, plot_h + 12.0);
            ctx.set_global_alpha(1.0);
            ctx.set_font("bold 8px ui-monospace, monospace");
            ctx.set_text_align("center");
            ctx.set_fill_style_str(C_ACCENT);
            let _ = ctx.fill_text("LIVE", x + w_slot / 2.0, plot_y - 12.0);
        }

        slots.push(Slot { t, x, w: w_slot, last: is_last, levels: level_ps });
    }

    // delta zero line
    ctx.set_stroke_style_str("#1c2a3c");
    ctx.begin_path();
    ctx.move_to(plot_x, delta_mid);
    ctx.line_to(plot_x + plot_w, delta_mid);
    ctx.stroke();

    // ----- last price line + tag -------------------------------------------
    if st.last_px > p_lo && st.last_px < p_hi {
        let y = p2y(st.last_px);
        let arr = js_sys::Array::new();
        arr.push(&2.0.into());
        arr.push(&3.0.into());
        let _ = ctx.set_line_dash(arr.as_ref());
        ctx.set_stroke_style_str("#97fce4");
        ctx.set_global_alpha(0.45);
        ctx.begin_path();
        ctx.move_to(plot_x, y);
        ctx.line_to(plot_x + plot_w, y);
        ctx.stroke();
        ctx.set_global_alpha(1.0);
        let empty = js_sys::Array::new();
        let _ = ctx.set_line_dash(empty.as_ref());
        let lbl = fmt_px(st.last_px);
        ctx.set_font("bold 9.5px ui-monospace, monospace");
        let tw = ctx.measure_text(&lbl).map(|m| m.width()).unwrap_or(52.0) + 10.0;
        ctx.set_fill_style_str("#04070c");
        ctx.fill_rect(cw - AXIS_W + 2.0, y - 8.0, tw + 4.0, 16.0);
        ctx.set_fill_style_str(C_ACCENT);
        ctx.set_text_align("left");
        let _ = ctx.fill_text(&lbl, cw - AXIS_W + 5.0, y);
    }

    // ----- chart header line -------------------------------------------------
    ctx.set_font("10px ui-monospace, monospace");
    ctx.set_text_align("left");
    ctx.set_text_baseline("alphabetic");
    ctx.set_fill_style_str("#e8eff7");
    let span = if n > 1 {
        (vis[n - 1].0 - vis[0].0) / 60_000
    } else {
        st.interval / 60_000
    };
    let _ = ctx.fill_text(&format!("{} · {}", st.coin, iv_label(st.interval)), plot_x + 2.0, 13.0);
    ctx.set_fill_style_str("#54677d");
    let _ = ctx.fill_text(
        &format!(
            "levels ±{} · {} windows (~{}m)",
            fmt_k(st.bucket),
            n,
            span.max(0)
        ),
        plot_x + 130.0,
        13.0,
    );

    // ----- crosshair ----------------------------------------------------------
    if let Some((mx, my)) = st.mouse {
        if mx >= plot_x && mx <= plot_x + plot_w && my >= plot_y && my <= delta_y + DELTA_H {
            let slot = slots.iter().find(|s| mx >= s.x - 2.0 && mx <= s.x + s.w + 2.0);
            ctx.set_stroke_style_str("#31465f");
            ctx.set_line_width(1.0);
            if let Some(s) = slot {
                ctx.begin_path();
                ctx.move_to(s.x + s.w / 2.0, plot_y - 6.0);
                ctx.line_to(s.x + s.w / 2.0, delta_y + DELTA_H);
                ctx.stroke();
            }
            if my >= plot_y && my <= plot_y + plot_h {
                ctx.begin_path();
                ctx.move_to(plot_x, my);
                ctx.line_to(plot_x + plot_w, my);
                ctx.stroke();
                // hovered level highlight
                if let Some(lp) = nearest_level_in(&slots, mx, my, p2y) {
                    let s = slots.iter().find(|s| mx >= s.x - 2.0 && mx <= s.x + s.w + 2.0);
                    if let Some(s) = s {
                        let y = p2y(lp);
                        ctx.set_stroke_style_str("#e8eff7");
                        ctx.set_global_alpha(0.7);
                        ctx.stroke_rect(s.x + 15.0, y - 7.5, s.w - 16.0, 15.0);
                        ctx.set_global_alpha(1.0);
                    }
                }
                // price tag on axis
                let p = p_hi - (my - plot_y) / plot_h * (p_hi - p_lo);
                ctx.set_font("9px ui-monospace, monospace");
                ctx.set_fill_style_str("#04070c");
                ctx.fill_rect(cw - AXIS_W + 2.0, my - 7.0, 54.0, 14.0);
                ctx.set_fill_style_str("#8fa3ba");
                ctx.set_text_align("left");
                let _ = ctx.fill_text(&fmt_px(p), cw - AXIS_W + 5.0, my);
            }
        }
    }

    LAYOUT.with(|c| {
        *c.borrow_mut() = Some(Layout {
            plot_x,
            plot_w,
            plot_y,
            plot_h,
            price_lo: p_lo,
            price_hi: p_hi,
            slots,
            bucket: st.bucket,
        })
    });
}

thread_local! {
    static LAYOUT: std::cell::RefCell<Option<Layout>> = const { std::cell::RefCell::new(None) };
}

fn nearest_level_in(slots: &[Slot], x: f64, y: f64, p2y: impl Fn(f64) -> f64) -> Option<f64> {
    let s = slots.iter().find(|s| x >= s.x - 2.0 && x <= s.x + s.w + 2.0)?;
    let mut best: Option<(f64, f64)> = None;
    for lp in &s.levels {
        let d = (p2y(*lp) - y).abs();
        if d <= 8.0 && best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, *lp));
        }
    }
    best.map(|(_, lp)| lp)
}

pub fn iv_label(iv: i64) -> &'static str {
    match iv {
        15_000 => "15s",
        60_000 => "1m",
        300_000 => "5m",
        900_000 => "15m",
        3_600_000 => "1h",
        _ => "1m",
    }
}
