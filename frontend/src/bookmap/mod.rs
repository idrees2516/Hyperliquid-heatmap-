//! Bookmap-style liquidity terminal: live L2 heatmap, trade bubbles,
//! liquidation markers, volume profile, DOM ladder, depth chart and a
//! delta/CVD strip — rendered on a single canvas.
//!
//! The heatmap is cached in an offscreen canvas: new book columns shift the
//! image left and paint only the fresh strip (bookmap's own trick), while
//! view changes (zoom / price range / intensity / resize) trigger one full
//! rebuild. Overlays are redrawn every frame on top.

use std::collections::VecDeque;

use shared::{
    BookCol, BookPayload, FootWindow, Intent, LevelState, LiveLiquidation, LiveTrade,
};
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

use crate::ui::{fmt_k, fmt_px, ms_hm};

/// time-axis label: seconds when the step is finer than a minute
fn time_label(t: i64, step_ms: f64) -> String {
    if step_ms < 60_000.0 {
        let s = (t / 1000) % 60;
        format!("{}:{:02}", ms_hm(t), s)
    } else {
        ms_hm(t)
    }
}

// ---------------------------------------------------------------------------
// colour ramp (log-normalized resting size → bookmap heat)
// ---------------------------------------------------------------------------

/// reference-terminal ramp (dark): navy -> blue -> cyan -> white ->
/// yellow -> orange -> red; the zero colour equals the pane background.
const RAMP: [&str; 16] = [
    "#050f1f",
    "#071c35",
    "#08294a",
    "#0a3660",
    "#0c4979",
    "#105e94",
    "#1373b0",
    "#3090c7",
    "#57b0db",
    "#7fd0f0",
    "#cdedfa",
    "#f8f2c3",
    "#ffe14a",
    "#ffa719",
    "#f76909",
    "#e8281c",
];

fn ramp(v: f64) -> &'static str {
    let i = ((v * 15.0).floor() as i32).clamp(0, 15) as usize;
    RAMP[i]
}

const BG: &str = "#050f1f";
const GRID: &str = "#0b1a30";
const C_DIM: &str = "#5b6b80";
const C_TXT: &str = "#c9d6e4";
const BUY: &str = "#1fe3a6";
const SELL: &str = "#ff4d6a";
const LIQ: &str = "#ffd24a";
const C_VA: &str = "#e879f9";
/// price-path line + session VWAP (screenshot pink)
const PINK: &str = "#ff4fd0";
const VWAP: &str = "#ff8fe0";

const FONT: &str = "10px ui-monospace, SFMono-Regular, Menlo, monospace";
const FONT_B: &str = "bold 10px ui-monospace, SFMono-Regular, Menlo, monospace";

/// full rebuild cadence: keeps the shifted image crisp
const REBUILD_EVERY_MS: f64 = 8_000.0;

// ---------------------------------------------------------------------------
// view state
// ---------------------------------------------------------------------------

pub struct BookView {
    pub coin: String,
    pub cols: VecDeque<BookCol>,
    /// latest live book (DOM ladder / depth source)
    pub cur: Option<BookCol>,
    /// REST-seeded prints (bubbles history); live prints append from the tape
    pub seed_trades: VecDeque<LiveTrade>,
    pub seed_liqs: VecDeque<LiveLiquidation>,
    pub fetched_at: f64,
    // view params
    pub vis_cols: usize,
    /// -1..1: shifts the colour window along the size axis
    pub intensity: f64,
    pub show_trades: bool,
    pub show_liqs: bool,
    pub show_profile: bool,
    pub show_dom: bool,
    pub show_radar: bool,
    pub show_delta: bool,
    pub show_vwap: bool,
    pub hover: Option<(f64, f64)>,
    pub px_lo: f64,
    pub px_hi: f64,
    pub heat_ms: f64,
    pub frame_ms: f64,
    pub rebuilds: u32,
    heat: Option<HeatCanvas>,
}

impl Default for BookView {
    fn default() -> Self {
        BookView {
            coin: "BTC".into(),
            cols: VecDeque::new(),
            cur: None,
            seed_trades: VecDeque::new(),
            seed_liqs: VecDeque::new(),
            fetched_at: 0.0,
            vis_cols: 1_600,
            intensity: 0.0,
            show_trades: true,
            show_liqs: true,
            show_profile: true,
            show_dom: true,
            show_radar: true,
            show_delta: true,
            show_vwap: true,
            hover: None,
            px_lo: 0.0,
            px_hi: 0.0,
            heat_ms: 0.0,
            frame_ms: 0.0,
            rebuilds: 0,
            heat: None,
        }
    }
}

impl BookView {
    pub fn reset(&mut self, coin: &str) {
        self.coin = coin.to_string();
        self.cols.clear();
        self.cur = None;
        self.seed_trades.clear();
        self.seed_liqs.clear();
        self.fetched_at = 0.0;
        self.px_lo = 0.0;
        self.px_hi = 0.0;
        self.heat = None;
    }

    pub fn stale(&self, coin: &str) -> bool {
        self.coin != coin || js_sys::Date::now() - self.fetched_at > 20_000.0
    }

    pub fn on_seed(&mut self, p: BookPayload) {
        if p.coin != self.coin {
            return;
        }
        self.cols = p.cols.into();
        if p.book.is_some() {
            self.cur = p.book;
        }
        self.seed_trades = p.trades.into();
        self.seed_liqs = p.liqs.into();
        self.fetched_at = js_sys::Date::now();
        self.heat = None; // force full rebuild
    }

    pub fn on_col(&mut self, coin: &str, col: BookCol) {
        if coin != self.coin {
            return;
        }
        if let Some(last) = self.cols.back() {
            if col.t <= last.t {
                return;
            }
        }
        self.cols.push_back(col);
        while self.cols.len() > 7_400 {
            self.cols.pop_front();
        }
    }

    pub fn on_live(&mut self, coin: &str, book: BookCol) {
        if coin == self.coin {
            self.cur = Some(book);
        }
    }

    /// drop the heatmap cache (forces a full redraw)
    pub fn invalidate(&mut self) {
        self.heat = None;
    }

    /// back to the live edge: default zoom, auto price fit, neutral heat
    pub fn recenter(&mut self) {
        self.vis_cols = 1_800;
        self.px_lo = 0.0;
        self.px_hi = 0.0;
        self.intensity = 0.0;
        self.heat = None;
    }

    pub fn set_intensity(&mut self, v: f64) {
        if (self.intensity - v).abs() > 1e-6 {
            self.intensity = v;
        }
    }
}

// ---------------------------------------------------------------------------
// heatmap cache
// ---------------------------------------------------------------------------

struct HeatCanvas {
    el: HtmlCanvasElement,
    ctx: CanvasRenderingContext2d,
    w: f64,
    h: f64,
    // signature of the last full rebuild
    sig_coin: String,
    sig_px_lo: f64,
    sig_px_hi: f64,
    sig_vis: usize,
    sig_wlo: f64,
    sig_whi: f64,
    last_col_t: i64,
    last_rebuild_at: f64,
    next_pct_at: f64,
}

impl HeatCanvas {
    fn new(w: f64, h: f64, dpr: f64) -> Self {
        let el = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .create_element("canvas")
            .unwrap()
            .dyn_into::<HtmlCanvasElement>()
            .unwrap();
        el.set_width((w * dpr) as u32);
        el.set_height((h * dpr) as u32);
        let ctx = el
            .get_context("2d")
            .unwrap()
            .unwrap()
            .dyn_into::<CanvasRenderingContext2d>()
            .unwrap();
        let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
        HeatCanvas {
            el,
            ctx,
            w,
            h,
            sig_coin: String::new(),
            sig_px_lo: 0.0,
            sig_px_hi: 0.0,
            sig_vis: 0,
            sig_wlo: 0.0,
            sig_whi: 0.0,
            last_col_t: 0,
            last_rebuild_at: 0.0,
            next_pct_at: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// layout
// ---------------------------------------------------------------------------

struct Lay {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    liq_x: f64,
    liq_w: f64,
    prof_x: f64,
    prof_w: f64,
    axis_x: f64,
    axis_w: f64,
    dom_x: f64,
    dom_w: f64,
    delta_y: f64,
    delta_h: f64,
    time_y: f64,
}

impl Lay {
    fn p2y(&self, px: f64, lo: f64, hi: f64) -> f64 {
        if hi <= lo {
            return self.y + self.h / 2.0;
        }
        self.y + (hi - px) / (hi - lo) * self.h
    }
    fn y2p(&self, y: f64, lo: f64, hi: f64) -> f64 {
        if hi <= lo {
            return 0.0;
        }
        hi - (y - self.y) / self.h * (hi - lo)
    }
}

fn dash(ctx: &CanvasRenderingContext2d, on: bool, a: f64, b: f64) {
    let arr = js_sys::Array::new();
    if on {
        arr.push(&a.into());
        arr.push(&b.into());
    }
    let _ = ctx.set_line_dash(arr.as_ref());
}

/// x position of a timestamp, snapped to the nearest sampled column
/// (columns are index-spaced, not time-uniform, so overlays must follow
/// the grid to stay aligned with the heatmap).
fn col_x(cols: &VecDeque<BookCol>, t: i64, col_w: f64, w: f64) -> f64 {
    let n = cols.len();
    if n == 0 {
        return w;
    }
    // binary search: last column with ct <= t
    let (mut lo, mut hi) = (0usize, n); // hi exclusive
    while lo < hi {
        let mid = (lo + hi) / 2;
        if cols[mid].t <= t {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let j = lo.saturating_sub(1); // index of column at-or-before t
    // snap to the closer of j / j+1
    let j2 = if j + 1 < n && (t - cols[j].t) > (cols[j + 1].t - t) {
        j + 1
    } else {
        j
    };
    w - (n as f64 - j2 as f64) * col_w - col_w * 0.5
}

// ---------------------------------------------------------------------------
// draw entry
// ---------------------------------------------------------------------------

pub fn draw(
    bv: &mut BookView,
    tape: &VecDeque<LiveTrade>,
    liq_feed: &VecDeque<LiveLiquidation>,
    windows: &[FootWindow],
    radar: &Option<shared::RadarSnapshot>,
    canvas: &HtmlCanvasElement,
) {
    let t0 = js_sys::Date::now();
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
    if cw < 320.0 || ch < 240.0 {
        return;
    }
    let w_px = (cw * dpr) as u32;
    let h_px = (ch * dpr) as u32;
    if canvas.width() != w_px || canvas.height() != h_px {
        canvas.set_width(w_px);
        canvas.set_height(h_px);
        bv.heat = None;
    }
    let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
    ctx.set_fill_style_str(BG);
    ctx.fill_rect(0.0, 0.0, cw, ch);

    // ----- layout ---------------------------------------------------------
    let top_h = 26.0;
    let time_h = 18.0;
    let delta_h = if bv.show_delta { 54.0 } else { 0.0 };
    let axis_w = 62.0;
    let liq_w = if bv.show_radar { 78.0 } else { 0.0 };
    let prof_w = if bv.show_profile { 86.0 } else { 0.0 };
    let dom_w = if bv.show_dom { (cw * 0.19).clamp(170.0, 300.0) } else { 0.0 };
    let plot_x = 0.0;
    let plot_w = (cw - liq_w - prof_w - axis_w - dom_w - 4.0).max(120.0);
    let plot_y = top_h;
    let plot_h = (ch - top_h - time_h - delta_h - 4.0).max(80.0);
    let lay = Lay {
        x: plot_x,
        y: plot_y,
        w: plot_w,
        h: plot_h,
        liq_x: plot_x + plot_w,
        liq_w,
        prof_x: plot_x + plot_w + liq_w,
        prof_w,
        axis_x: plot_x + plot_w + liq_w + prof_w,
        axis_w,
        dom_x: plot_x + plot_w + liq_w + prof_w + axis_w,
        dom_w,
        delta_y: plot_y + plot_h + 4.0,
        delta_h,
        time_y: ch - time_h,
    };

    if bv.cols.is_empty() {
        ctx.set_fill_style_str(C_DIM);
        ctx.set_font("13px ui-monospace, monospace");
        ctx.set_text_align("center");
        let _ = ctx.fill_text(
            "streaming live order book … (a heatmap column arrives every 400 ms)",
            cw / 2.0,
            ch / 2.0,
        );
        ctx.set_text_align("left");
        draw_status(&ctx, bv, cw, 0, 0);
        return;
    }

    // ----- visible columns -------------------------------------------------
    let n = bv.cols.len();
    let vis_n = bv.vis_cols.min(n);
    let skip = n - vis_n;
    let first_t = bv.cols[skip].t;
    let last_t = bv.cols[n - 1].t;
    // auto-fit while history is shorter than the zoom window: stretch the
    // available columns across the full width, then scroll once saturated
    let col_w = (lay.w / vis_n.max(1) as f64).max(0.75);

    // ----- price range (auto, with hysteresis) ------------------------------
    let trades_all: Vec<&LiveTrade> = bv.seed_trades.iter().chain(tape.iter()).collect();
    let mut lo = f64::MAX;
    let mut hi = f64::MIN;
    let step_by = (vis_n / 320).max(1);
    for c in bv.cols.iter().skip(skip).step_by(step_by) {
        if let Some(b) = c.bids.first() {
            lo = lo.min(b.px);
        }
        if let Some(a) = c.asks.first() {
            hi = hi.max(a.px);
        }
    }
    for t in trades_all.iter() {
        if t.t >= first_t {
            lo = lo.min(t.px);
            hi = hi.max(t.px);
        }
    }
    // (no chart-range widening for magnet levels: the blue/red heat is the
    // hero and must fill the plot; the LIQ LEVELS panel shows the full
    // liquidation landscape on its own wider ±8% domain, and the radar
    // overlay still dashes any level that falls inside the traded range)
    if lo == f64::MAX || hi == f64::MIN || hi <= lo {
        let m = bv.cols[n - 1].mid();
        lo = m * 0.99;
        hi = m * 1.01;
    }
    let pad = (hi - lo) * 0.08 + lo * 0.0002;
    let t_lo = lo - pad;
    let t_hi = hi + pad;
    if bv.px_lo <= 0.0 || bv.px_hi <= bv.px_lo {
        bv.px_lo = t_lo;
        bv.px_hi = t_hi;
    } else {
        let cur = bv.px_hi - bv.px_lo;
        let tgt = t_hi - t_lo;
        if t_lo < bv.px_lo || t_hi > bv.px_hi || tgt < cur * 0.8 {
            bv.px_lo = t_lo;
            bv.px_hi = t_hi;
        }
    }
    let px_lo = bv.px_lo;
    let px_hi = bv.px_hi;
    let p2y = |px: f64| lay.p2y(px, px_lo, px_hi);
    let cols_ref = &bv.cols;
    let t2x = |t: i64| col_x(cols_ref, t, col_w, lay.w);

    // ----- heatmap ----------------------------------------------------------
    let heat_t0 = js_sys::Date::now();
    {
        let need_resize = bv
            .heat
            .as_ref()
            .map(|h| (h.w - lay.w).abs() > 0.5 || (h.h - lay.h).abs() > 0.5)
            .unwrap_or(true);
        if need_resize {
            bv.heat = None;
        }
        let heat = bv.heat.get_or_insert_with(|| HeatCanvas::new(lay.w, lay.h, dpr));
        let now = js_sys::Date::now();
        // percentiles of resting size over the visible window (log space)
        let mut wlo = heat.sig_wlo;
        let mut whi = heat.sig_whi;
        if heat.sig_coin != bv.coin
            || heat.sig_vis != vis_n
            || now >= heat.next_pct_at
            || heat.sig_wlo <= 0.0
        {
            let mut samples: Vec<f64> = Vec::with_capacity(4_096);
            for c in bv.cols.iter().skip(skip).step_by(9) {
                for l in c.bids.iter().step_by(2) {
                    samples.push(l.px * l.sz);
                }
                for l in c.asks.iter().step_by(2) {
                    samples.push(l.px * l.sz);
                }
            }
            if samples.len() >= 24 {
                samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let pick = |q: f64| samples[((samples.len() as f64 - 1.0) * q) as usize].max(1.0);
                wlo = pick(0.15);
                whi = pick(0.94).max(wlo * 1.6);
            } else {
                wlo = 2_000.0;
                whi = 400_000.0;
            }
            // intensity slider shifts the window along the size axis
            let lg_lo = wlo.ln();
            let lg_hi = whi.ln();
            let shift = -bv.intensity * 0.8 * (lg_hi - lg_lo);
            wlo = (lg_lo + shift).exp();
            whi = (lg_hi + shift).exp();
            heat.next_pct_at = now + 4_000.0;
        }
        let full_rebuild = heat.sig_coin != bv.coin
            || heat.sig_px_lo != px_lo
            || heat.sig_px_hi != px_hi
            || heat.sig_vis != vis_n
            || heat.sig_wlo != wlo
            || heat.sig_whi != whi
            || heat.last_col_t == 0
            || heat.last_col_t > last_t
            || now - heat.last_rebuild_at > REBUILD_EVERY_MS;
        if full_rebuild {
            heat.ctx.set_fill_style_str(BG);
            heat.ctx.fill_rect(0.0, 0.0, lay.w, lay.h);
            for (i, c) in bv.cols.iter().skip(skip).enumerate() {
                let x = lay.w - (vis_n - i) as f64 * col_w;
                draw_column(&heat.ctx, c, x, col_w, &lay, px_lo, px_hi, wlo, whi);
            }
            heat.sig_coin = bv.coin.clone();
            heat.sig_px_lo = px_lo;
            heat.sig_px_hi = px_hi;
            heat.sig_vis = vis_n;
            heat.sig_wlo = wlo;
            heat.sig_whi = whi;
            heat.last_col_t = last_t;
            heat.last_rebuild_at = now;
            bv.rebuilds += 1;
        } else if last_t > heat.last_col_t {
            // incremental: shift the image left, paint only the new columns
            let new_cols = bv
                .cols
                .iter()
                .rev()
                .take_while(|c| c.t > heat.last_col_t)
                .count()
                .min(vis_n);
            let dx = new_cols as f64 * col_w;
            let _ = heat.ctx.set_image_smoothing_enabled(false);
            let _ = heat.ctx.draw_image_with_html_canvas_element_and_dw_and_dh(
                &heat.el,
                -dx,
                0.0,
                lay.w,
                lay.h,
            );
            heat.ctx.set_fill_style_str(BG);
            heat.ctx.fill_rect(lay.w - dx - col_w - 1.0, 0.0, dx + col_w + 2.0, lay.h);
            for (k, c) in bv.cols.iter().skip(n - new_cols).enumerate() {
                let x = lay.w - (new_cols - k) as f64 * col_w;
                draw_column(&heat.ctx, c, x, col_w, &lay, px_lo, px_hi, wlo, whi);
            }
            heat.last_col_t = last_t;
        }
        let _ = ctx.draw_image_with_html_canvas_element_and_dw_and_dh(
            &heat.el,
            lay.x,
            lay.y,
            lay.w,
            lay.h,
        );
    }
    bv.heat_ms = js_sys::Date::now() - heat_t0;

    // ----- grid + axes ------------------------------------------------------
    draw_grid_and_axes(&ctx, &lay, px_lo, px_hi, &bv.cols, skip, vis_n, col_w);

    // ----- session VWAP with bands (screenshot pink) ------------------------
    if bv.show_vwap {
        draw_vwap(&ctx, &lay, &trades_all, first_t, &p2y, &t2x);
    }

    // ----- radar magnet levels ----------------------------------------------
    if bv.show_radar {
        if let Some(rs) = radar {
            draw_radar(&ctx, &lay, rs, px_lo, px_hi, &p2y);
        }
    }

    // ----- price line (last print per column) --------------------------------
    let mut last_px_seen = 0.0_f64;
    {
        let mut pts: Vec<(f64, f64)> = Vec::new();
        let mut last_x = -1.0_f64;
        let mut last_px = 0.0_f64;
        for t in trades_all.iter() {
            if t.t < first_t {
                continue;
            }
            let x = t2x(t.t);
            if (x - last_x).abs() >= 0.75 {
                if last_px > 0.0 {
                    pts.push((last_x, p2y(last_px)));
                }
                last_x = x;
            }
            last_px = t.px;
        }
        last_px_seen = last_px;
        if last_px > 0.0 {
            pts.push((last_x, p2y(last_px)));
        }
        if pts.len() >= 2 {
            // dark casing under the pink path so it reads over bright heat
            ctx.set_line_join("round");
            ctx.set_stroke_style_str("rgba(5,10,20,0.6)");
            ctx.set_line_width(3.4);
            ctx.begin_path();
            for (i, (x, y)) in pts.iter().enumerate() {
                if i == 0 {
                    ctx.move_to(*x, *y);
                } else {
                    ctx.line_to(*x, *y);
                }
            }
            ctx.stroke();
            ctx.set_stroke_style_str(PINK);
            ctx.set_line_width(1.5);
            ctx.begin_path();
            for (i, (x, y)) in pts.iter().enumerate() {
                if i == 0 {
                    ctx.move_to(*x, *y);
                } else {
                    ctx.line_to(*x, *y);
                }
            }
            ctx.stroke();
        }
    }
    // last price tag on the axis
    if last_px_seen > 0.0 {
        let y = p2y(last_px_seen);
        let up = trades_all
            .iter()
            .rev()
            .find(|t| t.t >= first_t)
            .map(|t| t.side.is_buy())
            .unwrap_or(true);
        ctx.set_fill_style_str(if up { BUY } else { SELL });
        ctx.fill_rect(lay.axis_x + 1.0, y - 7.5, lay.axis_w - 2.0, 15.0);
        ctx.set_fill_style_str("#04110c");
        ctx.set_font(FONT_B);
        ctx.set_text_align("center");
        let _ = ctx.fill_text(&fmt_px(last_px_seen), lay.axis_x + lay.axis_w / 2.0, y + 3.5);
        ctx.set_text_align("left");
        // candle-close countdown under the tag (next 1m boundary);
        // flips above the tag when the tag sits near the plot bottom
        let rem_ms = 60_000 - (js_sys::Date::now() as i64).rem_euclid(60_000);
        let cd = format!("{}:{:02}", rem_ms / 60_000, (rem_ms % 60_000) / 1_000);
        let below = y + 22.0 <= lay.y + lay.h;
        let cy = if below { y + 8.0 } else { y - 21.0 };
        ctx.set_fill_style_str("rgba(8,16,28,0.94)");
        ctx.fill_rect(lay.axis_x + 2.0, cy, lay.axis_w - 4.0, 13.0);
        ctx.set_stroke_style_str("rgba(30,48,70,0.8)");
        ctx.set_line_width(1.0);
        ctx.stroke_rect(lay.axis_x + 2.0, cy, lay.axis_w - 4.0, 13.0);
        ctx.set_fill_style_str(if up { "#9defd4" } else { "#ffb3c0" });
        ctx.set_font(FONT);
        ctx.set_text_align("center");
        let _ = ctx.fill_text(&cd, lay.axis_x + lay.axis_w / 2.0, cy + 9.5);
        ctx.set_text_align("left");
    }

    // ----- trade bubbles -----------------------------------------------------
    if bv.show_trades {
        let mut sizes: Vec<f64> = trades_all
            .iter()
            .filter(|t| t.t >= first_t)
            .map(|t| t.usd)
            .collect();
        sizes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p95 = if sizes.is_empty() {
            1.0
        } else {
            let idx = ((sizes.len() as f64) * 0.95) as usize;
            sizes[idx.min(sizes.len() - 1)].max(1.0)
        };
        for t in trades_all.iter() {
            if t.t < first_t {
                continue;
            }
            let x = t2x(t.t);
            if x < lay.x - 20.0 {
                continue;
            }
            let y = p2y(t.px);
            if y < lay.y - 20.0 || y > lay.y + lay.h + 20.0 {
                continue;
            }
            let is_liq = matches!(t.intent, Intent::Liquidation | Intent::Deleveraging);
            if is_liq && !bv.show_liqs {
                continue;
            }
            let r = (1.6 + 7.0 * (t.usd / p95).sqrt()).clamp(1.8, 16.0);
            // whale prints carry the venue tag (screenshot bubbles)
            let tagged = t.usd >= (p95 * 1.25).max(50_000.0);
            if is_liq {
                // gold diamond with glow
                ctx.set_fill_style_str("rgba(255,210,74,0.22)");
                ctx.begin_path();
                let _ = ctx.arc(x, y, r * 1.8, 0.0, std::f64::consts::TAU);
                let _ = ctx.fill();
                ctx.set_fill_style_str(LIQ);
                ctx.begin_path();
                ctx.move_to(x, y - r);
                ctx.line_to(x + r * 0.78, y);
                ctx.line_to(x, y + r);
                ctx.line_to(x - r * 0.78, y);
                ctx.close_path();
                let _ = ctx.fill();
                if t.usd >= 100_000.0 {
                    ctx.set_fill_style_str(LIQ);
                    ctx.set_font(FONT_B);
                    let _ = ctx.fill_text(&format!("HL {}", fmt_k(t.usd)), x + r + 3.0, y + 3.0);
                }
            } else {
                let buy = t.side.is_buy();
                let rr = if tagged { r.max(9.5) } else { r };
                ctx.set_fill_style_str(if buy {
                    "rgba(31,227,166,0.55)"
                } else {
                    "rgba(255,77,106,0.55)"
                });
                ctx.begin_path();
                let _ = ctx.arc(x, y, rr, 0.0, std::f64::consts::TAU);
                let _ = ctx.fill();
                ctx.set_stroke_style_str(if buy { BUY } else { SELL });
                ctx.set_line_width(1.0);
                ctx.stroke();
                if tagged {
                    ctx.set_fill_style_str("#ffffff");
                    ctx.set_font("bold 9px ui-monospace, SFMono-Regular, Menlo, monospace");
                    ctx.set_text_align("center");
                    let _ = ctx.fill_text("HL", x, y + 3.0);
                    ctx.set_text_align("left");
                }
            }
        }
        // standalone liquidation events (not tied to a print)
        if bv.show_liqs {
            for l in bv.seed_liqs.iter().chain(liq_feed.iter()) {
                if l.coin != bv.coin || l.t < first_t || l.usd < 10_000.0 {
                    continue;
                }
                let x = t2x(l.t);
                let y = p2y(l.px);
                ctx.set_stroke_style_str(LIQ);
                ctx.set_line_width(1.6);
                ctx.begin_path();
                let _ = ctx.arc(x, y, 9.0, 0.0, std::f64::consts::TAU);
                ctx.stroke();
                ctx.set_stroke_style_str("rgba(255,210,74,0.55)");
                ctx.begin_path();
                let _ = ctx.arc(x, y, 14.0, 0.0, std::f64::consts::TAU);
                ctx.stroke();
                ctx.set_fill_style_str(LIQ);
                ctx.set_font(FONT_B);
                ctx.set_text_align("center");
                let _ = ctx.fill_text("HL", x, y + 3.0);
                ctx.set_text_align("left");
            }
        }
    }

    // ----- volume profile -----------------------------------------------------
    if bv.show_profile && lay.prof_w > 8.0 {
        draw_profile(&ctx, &lay, &trades_all, first_t, px_lo, px_hi, &p2y);
    }

    // ----- LIQ LEVELS panel (green long / red short liquidation levels) ------
    if bv.show_radar && lay.liq_w > 8.0 {
        if let Some(rs) = radar {
            if rs.coin == bv.coin {
                draw_liq_levels(&ctx, &lay, rs);
            }
        }
    }

    // ----- DOM ladder + depth --------------------------------------------------
    if bv.show_dom && lay.dom_w > 120.0 {
        if let Some(book) = bv.cur.as_ref() {
            draw_dom(&ctx, &lay, book);
            draw_depth(&ctx, &lay, book);
        }
    }

    // ----- delta / CVD strip ----------------------------------------------------
    if bv.show_delta && delta_h > 0.0 {
        draw_delta(&ctx, &lay, windows, first_t, &t2x);
    }

    // ----- crosshair -------------------------------------------------------------
    if let Some((mx, my)) = bv.hover {
        if mx >= lay.x && mx <= lay.x + lay.w && my >= lay.y && my <= lay.y + lay.h {
            ctx.set_stroke_style_str("rgba(180,200,220,0.35)");
            ctx.set_line_width(1.0);
            dash(&ctx, true, 3.0, 3.0);
            ctx.begin_path();
            ctx.move_to(mx, lay.y);
            ctx.line_to(mx, lay.y + lay.h);
            ctx.move_to(lay.x, my);
            ctx.line_to(lay.x + lay.w, my);
            ctx.stroke();
            dash(&ctx, false, 0.0, 0.0);
            // readout: price + time + book size at the hovered row
            let px = lay.y2p(my, px_lo, px_hi);
            let t_at = last_t - (((lay.x + lay.w - mx) / col_w).round() as i64) * 400;
            let mut txt = format!("{}  {}", fmt_px(px), ms_hm(t_at));
            let frac = 1.0 - (mx - lay.x) / lay.w;
            let ti = skip + (frac * (vis_n - 1) as f64) as usize;
            if let Some(c) = bv.cols.get(ti) {
                let band = (px_hi - px_lo) * 0.0025;
                if let Some(l) = c.bids.iter().find(|l| (l.px - px).abs() <= band) {
                    txt.push_str(&format!("  bid {} ({})", fmt_k(l.px * l.sz), l.n as u32));
                }
                if let Some(l) = c.asks.iter().find(|l| (l.px - px).abs() <= band) {
                    txt.push_str(&format!("  ask {} ({})", fmt_k(l.px * l.sz), l.n as u32));
                }
            }
            let w = txt.len() as f64 * 5.7 + 12.0;
            let bx = (mx + 8.0).min(lay.x + lay.w - w).max(lay.x);
            let by = (my - 24.0).max(lay.y + 2.0);
            ctx.set_fill_style_str("rgba(6,12,20,0.92)");
            ctx.fill_rect(bx, by, w, 16.0);
            ctx.set_stroke_style_str("rgba(120,150,180,0.35)");
            ctx.stroke_rect(bx, by, w, 16.0);
            ctx.set_fill_style_str(C_TXT);
            ctx.set_font(FONT);
            let _ = ctx.fill_text(&txt, bx + 6.0, by + 11.5);
        }
    }

    // ----- live-trades chip (bottom-right of the heat, clear of the panels) ----
    {
        let pulse = 0.5 + 0.5 * ((js_sys::Date::now() / 700.0).sin());
        let cx = lay.x + lay.w - 62.0;
        let cy = lay.y + lay.h - 12.0;
        ctx.set_fill_style_str(&format!("rgba(52,240,160,{:.2})", 0.35 + 0.6 * pulse));
        ctx.begin_path();
        let _ = ctx.arc(cx, cy, 3.0, 0.0, std::f64::consts::TAU);
        let _ = ctx.fill();
        ctx.set_fill_style_str("rgba(205,225,240,0.85)");
        ctx.set_font("9px ui-monospace, SFMono-Regular, Menlo, monospace");
        ctx.set_text_align("left");
        let _ = ctx.fill_text("live trades", cx + 7.0, cy + 3.0);
    }

    draw_status(&ctx, bv, cw, first_t, vis_n);
    bv.frame_ms = js_sys::Date::now() - t0;
}

// ---------------------------------------------------------------------------
// pieces
// ---------------------------------------------------------------------------

fn draw_column(
    ctx: &CanvasRenderingContext2d,
    col: &BookCol,
    x: f64,
    col_w: f64,
    lay: &Lay,
    px_lo: f64,
    px_hi: f64,
    wlo: f64,
    whi: f64,
) {
    let lg_lo = wlo.ln();
    let lg_span = (whi.ln() - lg_lo).max(0.001);
    for side in [&col.bids, &col.asks] {
        if side.len() < 2 {
            continue;
        }
        for j in 0..side.len() {
            let l = side[j];
            let usd = l.px * l.sz;
            let v = ((usd.ln() - lg_lo) / lg_span).clamp(0.0, 1.0);
            if v <= 0.03 {
                continue;
            }
            // band between midpoints to neighbour levels
            let prev = if j > 0 {
                side[j - 1].px
            } else {
                2.0 * l.px - side[j + 1].px
            };
            let next = if j + 1 < side.len() {
                side[j + 1].px
            } else {
                2.0 * l.px - prev
            };
            let up = (prev - l.px).abs() * 0.5;
            let dn = (l.px - next).abs() * 0.5;
            let y_hi = lay.p2y(l.px + up, px_lo, px_hi);
            let y_lo = lay.p2y(l.px - dn, px_lo, px_hi);
            if y_hi > lay.h + 4.0 || y_lo < -4.0 {
                continue;
            }
            let y0 = y_hi.max(-2.0);
            let y1 = y_lo.min(lay.h + 2.0);
            let h = (y1 - y0).max(1.0);
            ctx.set_fill_style_str(ramp(v));
            ctx.fill_rect(x, y0, col_w + 0.6, h);
        }
    }
}

fn nice_step(raw: f64) -> f64 {
    if raw <= 0.0 {
        return 1.0;
    }
    let mag = 10f64.powf(raw.log10().floor());
    let nrm = raw / mag;
    let s = if nrm < 1.5 {
        1.0
    } else if nrm < 3.5 {
        2.0
    } else if nrm < 7.5 {
        5.0
    } else {
        10.0
    };
    s * mag
}

fn draw_grid_and_axes(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    px_lo: f64,
    px_hi: f64,
    cols: &VecDeque<BookCol>,
    skip: usize,
    vis_n: usize,
    col_w: f64,
) {
    // horizontal grid + price axis
    let span = px_hi - px_lo;
    let step = nice_step(span / (lay.h / 34.0).max(2.0));
    let mut p = (px_lo / step).ceil() * step;
    ctx.set_font(FONT);
    while p < px_hi {
        let y = lay.p2y(p, px_lo, px_hi);
        ctx.set_stroke_style_str(GRID);
        ctx.set_line_width(1.0);
        ctx.begin_path();
        ctx.move_to(lay.x, y);
        ctx.line_to(lay.x + lay.w, y);
        ctx.stroke();
        ctx.set_fill_style_str(C_DIM);
        ctx.set_text_align("left");
        let _ = ctx.fill_text(&fmt_px(p), lay.axis_x + 3.0, y + 3.5);
        p += step;
    }
    // time axis — labels on the column grid (uniform spacing), seconds
    // shown when the per-label span is under a minute
    let step_cols = ((80.0 / col_w).ceil() as usize).max(1);
    let approx_ms = step_cols as f64 * 400.0;
    ctx.set_font(FONT);
    let mut i = 0usize;
    while i < vis_n {
        let c = &cols[skip + i];
        let x = lay.w - (vis_n - i) as f64 * col_w;
        if x > lay.x + 26.0 && x < lay.x + lay.w - 26.0 {
            ctx.set_stroke_style_str(GRID);
            ctx.begin_path();
            ctx.move_to(x, lay.y);
            ctx.line_to(x, lay.y + lay.h);
            ctx.stroke();
            ctx.set_fill_style_str(C_DIM);
            ctx.set_text_align("center");
            let _ = ctx.fill_text(&time_label(c.t, approx_ms), x, lay.time_y + 13.0);
        }
        i += step_cols;
    }
    ctx.set_text_align("left");
    // panel separators
    ctx.set_stroke_style_str("#141d2a");
    ctx.begin_path();
    if lay.liq_w > 0.0 {
        ctx.move_to(lay.liq_x + 0.5, lay.y);
        ctx.line_to(lay.liq_x + 0.5, lay.y + lay.h);
    }
    if lay.prof_w > 0.0 {
        ctx.move_to(lay.prof_x + 0.5, lay.y);
        ctx.line_to(lay.prof_x + 0.5, lay.y + lay.h);
    }
    ctx.move_to(lay.axis_x + 0.5, lay.y);
    ctx.line_to(lay.axis_x + 0.5, lay.y + lay.h);
    if lay.dom_w > 0.0 {
        ctx.move_to(lay.dom_x + 0.5, lay.y);
        ctx.line_to(lay.dom_x + 0.5, lay.y + lay.h);
    }
    if lay.delta_h > 0.0 {
        ctx.move_to(lay.x, lay.delta_y + 0.5);
        ctx.line_to(lay.x + lay.w, lay.delta_y + 0.5);
    }
    ctx.stroke();
}

/// Session VWAP with ±1σ bands (pink, like the reference screenshots):
/// running USD-weighted mean and variance over the visible prints.
fn draw_vwap(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    trades: &[&LiveTrade],
    first_t: i64,
    p2y: &dyn Fn(f64) -> f64,
    t2x: &dyn Fn(i64) -> f64,
) {
    let _ = lay;
    let mut sum_w = 0.0_f64;
    let mut sum_wp = 0.0_f64;
    let mut sum_wp2 = 0.0_f64;
    // (x, vwap_y, +σ_y, -σ_y)
    let mut band: Vec<(f64, f64, f64, f64)> = Vec::new();
    let mut last_x = -2.0_f64;
    for t in trades.iter() {
        if t.t < first_t || t.px <= 0.0 || t.usd <= 0.0 {
            continue;
        }
        sum_w += t.usd;
        sum_wp += t.usd * t.px;
        sum_wp2 += t.usd * t.px * t.px;
        if sum_w <= 0.0 {
            continue;
        }
        let vwap = sum_wp / sum_w;
        let var = (sum_wp2 / sum_w - vwap * vwap).max(0.0);
        let sig = var.sqrt();
        let x = t2x(t.t);
        if x - last_x >= 2.0 {
            band.push((x, p2y(vwap), p2y(vwap + sig), p2y(vwap - sig)));
            last_x = x;
        }
    }
    if band.len() < 2 {
        return;
    }
    // band fill
    ctx.set_fill_style_str("rgba(255,79,208,0.14)");
    ctx.begin_path();
    let (x0, _, up0, _) = band[0];
    ctx.move_to(x0, up0);
    for (x, _, up, _) in band.iter() {
        ctx.line_to(*x, *up);
    }
    for (x, _, _, dn) in band.iter().rev() {
        ctx.line_to(*x, *dn);
    }
    ctx.close_path();
    let _ = ctx.fill();
    // vwap line
    ctx.set_stroke_style_str(VWAP);
    ctx.set_line_width(1.4);
    dash(ctx, true, 5.0, 4.0);
    ctx.begin_path();
    let (x0, v0, _, _) = band[0];
    ctx.move_to(x0, v0);
    for (x, v, _, _) in band.iter() {
        ctx.line_to(*x, *v);
    }
    ctx.stroke();
    dash(ctx, false, 0.0, 0.0);
    // right-edge label
    let (lx, _, lv, _) = *band.last().unwrap();
    ctx.set_font(FONT_B);
    ctx.set_fill_style_str(VWAP);
    ctx.set_text_align("right");
    let _ = ctx.fill_text("VWAP", lx - 3.0, lv - 4.0);
    ctx.set_text_align("left");
    ctx.set_font(FONT);
}

fn draw_radar(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    rs: &shared::RadarSnapshot,
    px_lo: f64,
    px_hi: f64,
    p2y: &dyn Fn(f64) -> f64,
) {
    for lv in rs.levels.iter().filter(|l| l.est_usd > 20_000.0) {
        if lv.px < px_lo || lv.px > px_hi {
            continue;
        }
        let y = p2y(lv.px);
        let col = match lv.state {
            LevelState::Engaged => "#ff5c33",
            LevelState::Armed => {
                if lv.long {
                    "rgba(255,106,61,0.72)"
                } else {
                    "rgba(46,230,200,0.72)"
                }
            }
            LevelState::Swept => continue,
        };
        ctx.set_stroke_style_str(col);
        ctx.set_line_width(1.0);
        dash(ctx, true, 6.0, 4.0);
        ctx.begin_path();
        ctx.move_to(lay.x, y);
        ctx.line_to(lay.x + lay.w, y);
        ctx.stroke();
        dash(ctx, false, 0.0, 0.0);
        let lbl = format!(
            "{} {} {}",
            if lv.long { "L-LIQ" } else { "S-LIQ" },
            fmt_k(lv.est_usd),
            lv.state.label()
        );
        ctx.set_font(FONT);
        ctx.set_text_align("left");
        let w = lbl.len() as f64 * 5.7 + 8.0;
        ctx.set_fill_style_str("rgba(4,8,14,0.75)");
        ctx.fill_rect(lay.x + 4.0, y - 7.0, w, 13.0);
        ctx.set_fill_style_str(col);
        let _ = ctx.fill_text(&lbl, lay.x + 8.0, y + 3.5);
    }
}

/// Right-hand LIQ LEVELS panel: horizontal bars of estimated forced-flow
/// per price row — green = long liquidations, red = short liquidations —
/// with LEVEL MAX / CUM MAX stats, like the reference screenshots.
/// The panel maps its own wider price domain (levels within ±8% of the
/// last price) so the full liquidation landscape stays visible even when
/// the chart itself is zoomed tight around the traded range.
fn draw_liq_levels(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    rs: &shared::RadarSnapshot,
) {
    let x0 = lay.liq_x + 3.0;
    let w_in = lay.liq_w - 6.0;
    // header block (title + LEVEL MAX / CUM MAX per side)
    let hdr_h = 64.0;
    ctx.set_fill_style_str("rgba(4,10,20,0.72)");
    ctx.fill_rect(lay.liq_x, lay.y, lay.liq_w, lay.h);
    ctx.set_font("bold 9px ui-monospace, SFMono-Regular, Menlo, monospace");
    ctx.set_fill_style_str("#8fb7d4");
    ctx.set_text_align("left");
    let _ = ctx.fill_text("LIQ LEVELS", x0, lay.y + 12.0);

    let mid = if rs.px > 0.0 { rs.px } else { return };
    // own domain: levels within ±8% of price, always containing the price
    let mut dom_lo = mid * 0.992;
    let mut dom_hi = mid * 1.008;
    for lv in rs.levels.iter() {
        if lv.est_usd < 10_000.0 || (lv.px - mid).abs() / mid > 0.08 {
            continue;
        }
        dom_lo = dom_lo.min(lv.px);
        dom_hi = dom_hi.max(lv.px);
    }
    let span = (dom_hi - dom_lo).max(1e-9);
    let body_y = lay.y + hdr_h;
    let body_h = (lay.h - hdr_h - 14.0).max(20.0);
    let rows = ((body_h / 3.0) as usize).clamp(16, 220);
    let row_h = body_h / rows as f64;
    let mut long_rows = vec![0.0_f64; rows];
    let mut short_rows = vec![0.0_f64; rows];
    for lv in rs.levels.iter() {
        if lv.est_usd < 10_000.0 || lv.px < dom_lo || lv.px > dom_hi {
            continue;
        }
        let r = (((dom_hi - lv.px) / span) * rows as f64) as usize;
        if r < rows {
            if lv.long {
                long_rows[r] += lv.est_usd;
            } else {
                short_rows[r] += lv.est_usd;
            }
        }
    }
    let max_l = long_rows.iter().cloned().fold(0.0_f64, f64::max);
    let max_s = short_rows.iter().cloned().fold(0.0_f64, f64::max);
    let cum_l: f64 = long_rows.iter().sum();
    let cum_s: f64 = short_rows.iter().sum();

    // stats: two lines per side (LEVEL MAX + CUM MAX), green then red
    ctx.set_font("8px ui-monospace, SFMono-Regular, Menlo, monospace");
    ctx.set_fill_style_str("#34f0a0");
    let _ = ctx.fill_text(&format!("LEVEL MAX {}", fmt_k(max_l)), x0, lay.y + 23.0);
    let _ = ctx.fill_text(&format!("CUM MAX {}", fmt_k(cum_l)), x0, lay.y + 32.0);
    ctx.set_fill_style_str("#ff5c72");
    let _ = ctx.fill_text(&format!("LEVEL MAX {}", fmt_k(max_s)), x0, lay.y + 43.0);
    let _ = ctx.fill_text(&format!("CUM MAX {}", fmt_k(cum_s)), x0, lay.y + 52.0);
    // side markers + header separator
    ctx.set_fill_style_str("rgba(46,224,150,0.9)");
    ctx.fill_rect(x0, lay.y + 58.0, 5.0, 3.0);
    ctx.set_fill_style_str("rgba(255,77,96,0.9)");
    ctx.fill_rect(x0 + w_in - 5.0, lay.y + 58.0, 5.0, 3.0);
    ctx.set_stroke_style_str("#1a2942");
    ctx.set_line_width(1.0);
    ctx.begin_path();
    ctx.move_to(lay.liq_x, body_y - 4.5);
    ctx.line_to(lay.liq_x + lay.liq_w, body_y - 4.5);
    ctx.stroke();

    // bars: longs grow right from the left edge, shorts from the right edge
    for r in 0..rows {
        let y = body_y + r as f64 * row_h;
        if long_rows[r] > 0.0 && max_l > 0.0 {
            let bw = (long_rows[r] / max_l * (w_in * 0.5)).max(1.0);
            ctx.set_fill_style_str("rgba(46,224,150,0.85)");
            ctx.fill_rect(x0, y, bw, row_h.max(1.4));
        }
        if short_rows[r] > 0.0 && max_s > 0.0 {
            let bw = (short_rows[r] / max_s * (w_in * 0.5)).max(1.0);
            ctx.set_fill_style_str("rgba(255,77,96,0.85)");
            ctx.fill_rect(x0 + w_in - bw, y, bw, row_h.max(1.4));
        }
    }
    // current-price tick across the panel (orients longs below / shorts above)
    let py = body_y + ((dom_hi - mid) / span * body_h).clamp(4.0, body_h - 4.0);
    ctx.set_stroke_style_str("rgba(125,211,252,0.75)");
    ctx.set_line_width(1.0);
    ctx.begin_path();
    ctx.move_to(lay.liq_x + 1.0, py);
    ctx.line_to(lay.liq_x + lay.liq_w - 1.0, py);
    ctx.stroke();
    ctx.set_fill_style_str("#7dd3fc");
    ctx.set_font("7.5px ui-monospace, SFMono-Regular, Menlo, monospace");
    let _ = ctx.fill_text("price", lay.liq_x + 3.0, py - 2.5);
    // domain note at the bottom
    ctx.set_fill_style_str("rgba(91,107,128,0.9)");
    ctx.set_font("7.5px ui-monospace, SFMono-Regular, Menlo, monospace");
    let _ = ctx.fill_text(
        &format!("±8% {}–{}", fmt_k(dom_lo), fmt_k(dom_hi)),
        x0,
        lay.y + lay.h - 4.0,
    );
    ctx.set_font(FONT);
}

fn draw_profile(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    trades: &[&LiveTrade],
    first_t: i64,
    px_lo: f64,
    px_hi: f64,
    p2y: &dyn Fn(f64) -> f64,
) {
    let rows = ((lay.h / 3.0) as usize).clamp(24, 260);
    let span = (px_hi - px_lo).max(1e-9);
    let mut buy = vec![0.0_f64; rows];
    let mut sell = vec![0.0_f64; rows];
    for t in trades.iter() {
        if t.t < first_t {
            continue;
        }
        let r = (((px_hi - t.px) / span) * rows as f64) as usize;
        if r < rows {
            if t.side.is_buy() {
                buy[r] += t.usd;
            } else {
                sell[r] += t.usd;
            }
        }
    }
    let max_side = buy
        .iter()
        .chain(sell.iter())
        .cloned()
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let mid_x = lay.prof_x + lay.prof_w * 0.5;
    let row_h = lay.h / rows as f64;
    for r in 0..rows {
        let y = lay.y + r as f64 * row_h;
        let bw = buy[r] / max_side * (lay.prof_w * 0.5 - 4.0);
        let sw = sell[r] / max_side * (lay.prof_w * 0.5 - 4.0);
        if bw > 0.4 {
            ctx.set_fill_style_str("rgba(31,227,166,0.75)");
            ctx.fill_rect(mid_x, y, bw, row_h.max(1.5));
        }
        if sw > 0.4 {
            ctx.set_fill_style_str("rgba(255,77,106,0.75)");
            ctx.fill_rect(mid_x - sw, y, sw, row_h.max(1.5));
        }
    }
    // POC + value area
    let total: f64 = buy.iter().zip(sell.iter()).map(|(b, s)| b + s).sum();
    if total > 0.0 {
        let mut order: Vec<usize> = (0..rows).collect();
        order.sort_by(|&a, &b| {
            (buy[b] + sell[b])
                .partial_cmp(&(buy[a] + sell[a]))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let poc = order[0];
        let mut acc = buy[poc] + sell[poc];
        let mut va_lo = poc;
        let mut va_hi = poc;
        let mut i = 1;
        while acc < total * 0.7 && i < order.len() {
            let r = order[i];
            if r < va_lo {
                va_lo = r;
            }
            if r > va_hi {
                va_hi = r;
            }
            acc += buy[r] + sell[r];
            i += 1;
        }
        ctx.set_font(FONT_B);
        let poc_px = px_hi - (poc as f64 + 0.5) / rows as f64 * span;
        let va_hi_px = px_hi - va_lo as f64 / rows as f64 * span;
        let va_lo_px = px_hi - (va_hi + 1) as f64 / rows as f64 * span;
        // POC / VAH / VAL: dashed lines at their true prices, labels stacked
        // top→bottom with a minimum 11px gap so they never collide
        let mut labels: Vec<(f64, String, &str)> = vec![
            (p2y(va_hi_px), format!("VAH {}", fmt_px(va_hi_px)), C_VA),
            (p2y(poc_px), "POC".to_string(), "#ffffff"),
            (p2y(va_lo_px), format!("VAL {}", fmt_px(va_lo_px)), C_VA),
        ];
        labels.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut prev_text_y = -99.0_f64;
        for (ly, txt, col) in labels {
            let mut ty = ly - 3.0;
            if ty - prev_text_y < 11.0 {
                ty = prev_text_y + 11.0;
            }
            ty = ty.clamp(lay.y + 9.0, lay.y + lay.h - 3.0);
            prev_text_y = ty;
            ctx.set_stroke_style_str(if col == "#ffffff" {
                "rgba(255,255,255,0.65)"
            } else {
                col
            });
            ctx.set_line_width(1.0);
            dash(ctx, true, 4.0, 3.0);
            ctx.begin_path();
            ctx.move_to(lay.prof_x, ly);
            ctx.line_to(lay.prof_x + lay.prof_w, ly);
            ctx.stroke();
            dash(ctx, false, 0.0, 0.0);
            // backing box so guide lines never strike through the text
            let bw = txt.len() as f64 * 5.9 + 5.0;
            ctx.set_fill_style_str("rgba(4,10,20,0.88)");
            ctx.fill_rect(lay.prof_x + 1.0, ty - 8.5, bw, 11.0);
            ctx.set_fill_style_str(col);
            let _ = ctx.fill_text(&txt, lay.prof_x + 3.0, ty);
        }
    }
    ctx.set_font(FONT);
}

fn draw_dom(ctx: &CanvasRenderingContext2d, lay: &Lay, book: &BookCol) {
    let row_h = 15.0;
    let rows_per_side = (((lay.h * 0.44) / row_h) as usize).clamp(4, 26);
    let asks: Vec<&shared::BookLevel> = book.asks.iter().take(rows_per_side).collect();
    let bids: Vec<&shared::BookLevel> = book.bids.iter().take(rows_per_side).collect();
    let max_usd = asks
        .iter()
        .chain(bids.iter())
        .map(|l| l.px * l.sz)
        .fold(1.0_f64, f64::max);
    let bar_w = lay.dom_w * 0.5;
    let px_col = lay.dom_x + 6.0;
    let bar_col = px_col + 68.0;

    ctx.set_font(FONT_B);
    ctx.set_fill_style_str(C_DIM);
    ctx.set_text_align("left");
    let _ = ctx.fill_text("ORDER BOOK · live L2", lay.dom_x + 6.0, lay.y + 11.0);
    ctx.set_font(FONT);

    // asks: worst at top, best adjacent to the spread
    let mut y = lay.y + 18.0;
    for l in asks.iter().rev() {
        let usd = l.px * l.sz;
        let w = usd / max_usd * bar_w;
        ctx.set_fill_style_str("rgba(229,72,77,0.72)");
        ctx.fill_rect(bar_col, y, w, row_h - 2.0);
        if usd >= max_usd * 0.999 {
            ctx.set_stroke_style_str("#ff8f93");
            ctx.stroke_rect(bar_col, y, w, row_h - 2.0);
        }
        ctx.set_fill_style_str("#ff9b9f");
        ctx.set_text_align("right");
        let _ = ctx.fill_text(&fmt_px(l.px), px_col + 62.0, y + 10.0);
        ctx.set_fill_style_str(C_TXT);
        ctx.set_text_align("left");
        let _ = ctx.fill_text(&fmt_k(usd), bar_col + 4.0, y + 10.0);
        y += row_h;
    }
    // spread row
    let best_bid = book.bids.first().map(|l| l.px).unwrap_or(0.0);
    let best_ask = book.asks.first().map(|l| l.px).unwrap_or(0.0);
    let mid = (best_bid + best_ask) / 2.0;
    ctx.set_fill_style_str("rgba(20,32,46,0.95)");
    ctx.fill_rect(lay.dom_x + 2.0, y, lay.dom_w - 6.0, row_h + 1.0);
    ctx.set_font(FONT_B);
    ctx.set_fill_style_str("#7dd3fc");
    ctx.set_text_align("left");
    let _ = ctx.fill_text(
        &format!("{}  Δ{}bp", fmt_px(mid), book.spread_bps().round()),
        px_col,
        y + 11.0,
    );
    ctx.set_font(FONT);
    y += row_h + 2.0;
    // bids: best just below the spread
    for l in bids.iter() {
        let usd = l.px * l.sz;
        let w = usd / max_usd * bar_w;
        ctx.set_fill_style_str("rgba(31,200,150,0.72)");
        ctx.fill_rect(bar_col, y, w, row_h - 2.0);
        if usd >= max_usd * 0.999 {
            ctx.set_stroke_style_str("#4ef0be");
            ctx.stroke_rect(bar_col, y, w, row_h - 2.0);
        }
        ctx.set_fill_style_str("#67e8c3");
        ctx.set_text_align("right");
        let _ = ctx.fill_text(&fmt_px(l.px), px_col + 62.0, y + 10.0);
        ctx.set_fill_style_str(C_TXT);
        ctx.set_text_align("left");
        let _ = ctx.fill_text(&fmt_k(usd), bar_col + 4.0, y + 10.0);
        y += row_h;
    }
    ctx.set_text_align("left");
}

fn draw_depth(ctx: &CanvasRenderingContext2d, lay: &Lay, book: &BookCol) {
    let h = 92.0;
    let y0 = lay.y + lay.h - h;
    if y0 < lay.y + 150.0 {
        return;
    }
    let x0 = lay.dom_x + 6.0;
    let w = lay.dom_w - 12.0;
    let best_bid = book.bids.first().map(|l| l.px).unwrap_or(0.0);
    let best_ask = book.asks.first().map(|l| l.px).unwrap_or(0.0);
    if best_bid <= 0.0 || best_ask <= 0.0 {
        return;
    }
    let mid = (best_bid + best_ask) / 2.0;
    let lo = mid * 0.988;
    let hi = mid * 1.012;
    let mut cum_bid = 0.0;
    let mut bid_pts: Vec<(f64, f64)> = Vec::new();
    for l in book.bids.iter() {
        if l.px < lo {
            break;
        }
        cum_bid += l.px * l.sz;
        bid_pts.push((l.px, cum_bid));
    }
    let mut cum_ask = 0.0;
    let mut ask_pts: Vec<(f64, f64)> = Vec::new();
    for l in book.asks.iter() {
        if l.px > hi {
            break;
        }
        cum_ask += l.px * l.sz;
        ask_pts.push((l.px, cum_ask));
    }
    let max_cum = cum_bid.max(cum_ask).max(1.0);
    let px2x = |p: f64| x0 + (p - lo) / (hi - lo) * w;
    let v2y = |v: f64| y0 + h - 6.0 - v / max_cum * (h - 22.0);

    ctx.set_fill_style_str("rgba(5,9,15,0.9)");
    ctx.fill_rect(x0, y0, w, h);
    ctx.set_stroke_style_str("#141d2a");
    ctx.stroke_rect(x0, y0, w, h);
    ctx.set_stroke_style_str("#2b3a4d");
    ctx.begin_path();
    ctx.move_to(px2x(mid), y0);
    ctx.line_to(px2x(mid), y0 + h);
    ctx.stroke();
    let curve = |pts: &[(f64, f64)], col: &str| {
        if pts.len() < 2 {
            return;
        }
        ctx.set_stroke_style_str(col);
        ctx.set_line_width(1.4);
        ctx.begin_path();
        let (p0, _) = pts[0];
        ctx.move_to(px2x(p0), v2y(0.0));
        for (p, v) in pts.iter() {
            ctx.line_to(px2x(*p), v2y(*v));
        }
        let (pl, _) = *pts.last().unwrap();
        ctx.line_to(px2x(pl), v2y(0.0));
        ctx.stroke();
        ctx.set_fill_style_str(if col == BUY {
            "rgba(31,227,166,0.10)"
        } else {
            "rgba(255,77,106,0.10)"
        });
        ctx.begin_path();
        ctx.move_to(px2x(p0), y0 + h - 6.0);
        for (p, v) in pts.iter() {
            ctx.line_to(px2x(*p), v2y(*v));
        }
        ctx.line_to(px2x(pl), y0 + h - 6.0);
        ctx.close_path();
        let _ = ctx.fill();
    };
    curve(&bid_pts, BUY);
    curve(&ask_pts, SELL);
    ctx.set_font(FONT);
    ctx.set_fill_style_str(C_DIM);
    let _ = ctx.fill_text("DEPTH ±1.2%", x0 + 4.0, y0 + 11.0);
    ctx.set_fill_style_str(BUY);
    let _ = ctx.fill_text(&fmt_k(cum_bid), x0 + 4.0, y0 + h - 8.0);
    ctx.set_fill_style_str(SELL);
    ctx.set_text_align("right");
    let _ = ctx.fill_text(&fmt_k(cum_ask), x0 + w - 4.0, y0 + h - 8.0);
    ctx.set_text_align("left");
}

fn draw_delta(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    windows: &[FootWindow],
    first_t: i64,
    t2x: &dyn Fn(i64) -> f64,
) {
    let y0 = lay.delta_y;
    let h = lay.delta_h;
    let mid = y0 + h * 0.5;
    ctx.set_stroke_style_str(GRID);
    ctx.begin_path();
    ctx.move_to(lay.x, mid);
    ctx.line_to(lay.x + lay.w, mid);
    ctx.stroke();
    ctx.set_font(FONT);
    ctx.set_fill_style_str(C_DIM);
    let _ = ctx.fill_text("Δ / CVD (1m)", lay.x + 6.0, y0 + 10.0);
    let vis: Vec<&FootWindow> = windows.iter().filter(|w| w.t >= first_t - 60_000).collect();
    if vis.is_empty() {
        return;
    }
    let max_abs = vis.iter().map(|w| w.delta.abs()).fold(1.0_f64, f64::max);
    let bar_w = 14.0_f64.min((lay.w / vis.len().max(1) as f64) * 0.7).max(1.0);
    let total_delta: f64 = vis.iter().map(|w| w.delta).sum();
    let cvd_min = vis.iter().map(|w| w.delta).sum::<f64>().min(0.0);
    let cvd_span = (total_delta - cvd_min).abs().max(1.0);
    let mut cvd = 0.0;
    let mut cvd_pts: Vec<(f64, f64)> = Vec::new();
    for w in vis.iter() {
        let x = t2x(w.t + 30_000);
        if x < lay.x - 14.0 || x > lay.x + lay.w + 14.0 {
            cvd += w.delta;
            continue;
        }
        let bh = (w.delta / max_abs) * (h * 0.42);
        ctx.set_fill_style_str(if w.delta >= 0.0 {
            "rgba(31,227,166,0.75)"
        } else {
            "rgba(255,77,106,0.75)"
        });
        if bh >= 0.0 {
            ctx.fill_rect(x - bar_w / 2.0, mid - bh, bar_w, bh.max(1.0));
        } else {
            ctx.fill_rect(x - bar_w / 2.0, mid, bar_w, (-bh).max(1.0));
        }
        cvd += w.delta;
        let cy = y0 + h - 4.0 - ((cvd - cvd_min) / cvd_span) * (h - 12.0);
        cvd_pts.push((x, cy));
    }
    if cvd_pts.len() >= 2 {
        ctx.set_stroke_style_str("#ffd166");
        ctx.set_line_width(1.3);
        ctx.begin_path();
        for (i, (x, y)) in cvd_pts.iter().enumerate() {
            if i == 0 {
                ctx.move_to(*x, *y);
            } else {
                ctx.line_to(*x, *y);
            }
        }
        ctx.stroke();
    }
}

fn book_totals(book: &BookCol, frac: f64) -> (f64, f64) {
    let mid = book.mid();
    if mid <= 0.0 {
        return (0.0, 0.0);
    }
    let lo = mid * (1.0 - frac);
    let hi = mid * (1.0 + frac);
    let b: f64 = book
        .bids
        .iter()
        .take_while(|l| l.px >= lo)
        .map(|l| l.px * l.sz)
        .sum();
    let a: f64 = book
        .asks
        .iter()
        .take_while(|l| l.px <= hi)
        .map(|l| l.px * l.sz)
        .sum();
    (b, a)
}

fn draw_status(ctx: &CanvasRenderingContext2d, bv: &BookView, cw: f64, first_t: i64, vis_n: usize) {
    ctx.set_font(FONT_B);
    ctx.set_fill_style_str("#8fb7d4");
    let _ = ctx.fill_text(&format!("{} · LIVE BOOK", bv.coin), 8.0, 16.0);
    ctx.set_font(FONT);
    let mut x = 8.0 + bv.coin.len() as f64 * 7.4 + 78.0;
    if let Some(b) = bv.cur.as_ref() {
        let mid = b.mid();
        let (b1, a1) = book_totals(b, 0.01);
        let tot = b1 + a1;
        let imb = if tot > 0.0 { b1 / tot } else { 0.5 };
        ctx.set_fill_style_str("#7dd3fc");
        let _ = ctx.fill_text(&fmt_px(mid), x, 16.0);
        x += 64.0;
        ctx.set_fill_style_str(C_DIM);
        let _ = ctx.fill_text(&format!("spr {:.1}bp", b.spread_bps()), x, 16.0);
        x += 62.0;
        // imbalance bar
        ctx.set_fill_style_str("rgba(255,77,106,0.85)");
        ctx.fill_rect(x, 8.0, 70.0 * (1.0 - imb), 8.0);
        ctx.set_fill_style_str("rgba(31,227,166,0.85)");
        ctx.fill_rect(x + 70.0 * (1.0 - imb), 8.0, 70.0 * imb, 8.0);
        x += 78.0;
        ctx.set_fill_style_str(BUY);
        let s = format!("{} /", fmt_k(b1));
        let _ = ctx.fill_text(&s, x, 16.0);
        x += s.len() as f64 * 6.0 + 4.0;
        ctx.set_fill_style_str(SELL);
        let _ = ctx.fill_text(&fmt_k(a1), x, 16.0);
        x += 54.0;
        ctx.set_fill_style_str(C_DIM);
        if first_t > 0 {
            let mins = ((js_sys::Date::now() as i64 - first_t) as f64 / 60_000.0).round() as i64;
            let _ = ctx.fill_text(&format!("map {} cols · {}m hist", vis_n, mins), x, 16.0);
            x += 140.0;
        }
        let perf = format!(
            "heat {:.1}ms · frame {:.1}ms · rb {}",
            bv.heat_ms, bv.frame_ms, bv.rebuilds
        );
        let _ = ctx.fill_text(&perf, (x + 4.0).min((cw - perf.len() as f64 * 5.8).max(x)), 16.0);
    }
}

// ---------------------------------------------------------------------------
// input helpers (called from lib.rs)
// ---------------------------------------------------------------------------

pub fn wheel(bv: &mut BookView, dy: f64) {
    let f = if dy > 0.0 { 1.12 } else { 1.0 / 1.12 };
    let v = ((bv.vis_cols as f64 * f) as usize).clamp(80, 6_000);
    if v != bv.vis_cols {
        bv.vis_cols = v;
        bv.heat = None;
    }
}
