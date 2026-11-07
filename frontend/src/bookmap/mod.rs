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
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

use crate::ui::{fmt_k, fmt_px, ms_hm};

/// thousands-separated integer price for the right-edge pills (75622 -> 75,622)
fn fmt_comma(v: f64) -> String {
    if v >= 1000.0 {
        let n = v.round() as i64;
        let s = n.to_string();
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(bytes.len() + 4);
        for (i, c) in bytes.iter().enumerate() {
            if i > 0 && (bytes.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(*c as char);
        }
        out
    } else {
        fmt_px(v)
    }
}

/// "23h 28m ago" / "5m ago" / "42s ago"
fn fmt_ago(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 60 {
        format!("{}s ago", s)
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else {
        format!("{}h {}m ago", s / 3600, (s % 3600) / 60)
    }
}

/// local 12-hour clock time ("4:00 PM") via the JS Date
fn clock12(t: i64) -> String {
    let d = js_sys::Date::new(&JsValue::from_f64(t as f64));
    let h24 = d.get_hours();
    let m = d.get_minutes();
    let ampm = if h24 < 12 { "AM" } else { "PM" };
    let h12 = match h24 % 12 {
        0 => 12,
        h => h,
    };
    format!("{}:{:02} {}", h12, m, ampm)
}

/// filled rounded rectangle (TapeSurf pill)
fn rounded_rect(
    ctx: &CanvasRenderingContext2d,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    r: f64,
    fill: &str,
) {
    let r = r.min(w / 2.0).min(h / 2.0);
    ctx.begin_path();
    ctx.move_to(x + r, y);
    ctx.line_to(x + w - r, y);
    let _ = ctx.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    ctx.line_to(x + w, y + h - r);
    let _ = ctx.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    ctx.line_to(x + r, y + h);
    let _ = ctx.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    ctx.line_to(x, y + r);
    let _ = ctx.arc(x + r, y + r, r, std::f64::consts::PI, std::f64::consts::PI * 1.5);
    ctx.close_path();
    ctx.set_fill_style_str(fill);
    let _ = ctx.fill();
}

// ---------------------------------------------------------------------------
// colour ramp (log-normalized resting size → bookmap heat)
// ---------------------------------------------------------------------------

/// TapeSurf-style ask ramp (above price): navy -> blue -> cyan -> white;
/// the zero colour equals the pane background.
const RAMP_ASK: [&str; 14] = [
    "#070d18",
    "#081a30",
    "#0a2544",
    "#0c3358",
    "#0e446e",
    "#105a8a",
    "#1373a6",
    "#2b93c2",
    "#54b6da",
    "#84d6ea",
    "#b4e9f5",
    "#dcf7fb",
    "#eefbff",
    "#f6feff",
];

/// bid ramp (below price): dark green -> teal -> white-green
const RAMP_BID: [&str; 14] = [
    "#06110d",
    "#071f16",
    "#082b1e",
    "#0a3d29",
    "#0c5035",
    "#0f6843",
    "#128353",
    "#2aa36c",
    "#53c289",
    "#83dcab",
    "#b3ecc9",
    "#dbf8e6",
    "#eefdf4",
    "#f6fff9",
];

fn ramp_ask(v: f64) -> &'static str {
    let i = ((v * 13.0).floor() as i32).clamp(0, 13) as usize;
    RAMP_ASK[i]
}

fn ramp_bid(v: f64) -> &'static str {
    let i = ((v * 13.0).floor() as i32).clamp(0, 13) as usize;
    RAMP_BID[i]
}

const BG: &str = "#070d16";
const GRID: &str = "#0e1a28";

// ---------------------------------------------------------------------------
// heat window anchors — tuned for a DENSE map
// ---------------------------------------------------------------------------
// Percentiles of resting notional (log space) over the visible window that
// anchor the colour ramp. Reference terminal used p15..p94; that crushes the
// book's body to background and clips persistent walls at max red. Tuned:
//   - HEAT_PLO 0.15 -> 0.07 : more of the small/medium levels paint, so the
//     body of the book fills in (denser look, especially after a long
//     warm-up when the price has toured many resting bands)
//   - HEAT_PHI 0.94 -> 0.98 : more headroom, so whale walls spread across
//     white->yellow->red instead of all clipping at max
//   - HEAT_MIN_RATIO 1.6 -> 1.5 : slightly narrower log window = richer
//     mid-tones per notional step
const HEAT_PLO: f64 = 0.07;
const HEAT_PHI: f64 = 0.98;
const HEAT_MIN_RATIO: f64 = 1.5;
/// default zoom window in columns — 4500 × 400 ms = 30 min of heat (the
/// `30m` range preset), so a long warm-up shows as one dense TapeSurf-style
/// map; zoom out to ~45m (6800) for the full retained history
const DEF_VIS_COLS: usize = 4_500;
const C_DIM: &str = "#5b6b80";
const C_TXT: &str = "#c9d6e4";
const BUY: &str = "#1fe3a6";
const SELL: &str = "#ff4d6a";
const LIQ: &str = "#ffd24a";
const C_VA: &str = "#e879f9";
/// stepped traded-price path (TapeSurf signature yellow)
const YELLOW: &str = "#ffe14d";
/// 1m candles: teal-green up, red down
const CANDLE_UP: &str = "#17d3a1";
const CANDLE_DN: &str = "#ff4d5e";
/// session VWAP line (thin white, like the reference terminal)
const VWAP: &str = "#cdd8e2";

const FONT: &str = "10px ui-monospace, SFMono-Regular, Menlo, monospace";
const FONT_B: &str = "bold 10px ui-monospace, SFMono-Regular, Menlo, monospace";
/// 9px bold for the right-edge price pills (keeps them inside the axis)
const FONT_P: &str = "bold 9px ui-monospace, SFMono-Regular, Menlo, monospace";
/// 9px for panel header bands
const FONT_H: &str = "bold 9px ui-monospace, SFMono-Regular, Menlo, monospace";
/// 8px for stat lines / chips inside panels
const FONT_S: &str = "8px ui-monospace, SFMono-Regular, Menlo, monospace";
/// height of the collapsible panel header bands
const HDR_H: f64 = 17.0;
/// width of a collapsed panel rail (vertical title strip)
const RAIL_W: f64 = 15.0;
/// header band chrome
const HDR_BG: &str = "#0d1622";
const HDR_LN: &str = "#1c2c40";

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
    /// 1m candles overlay (TapeSurf-style teal/red)
    pub show_candles: bool,
    /// bottom volume histogram strip
    pub show_volume: bool,
    /// dotted wall tracks (biggest resting walls per column)
    pub show_trails: bool,
    // ---- TapeSurf-style collapsible right panels --------------------------
    /// LIQ LEVELS panel collapsed to a thin rail
    pub col_liq: bool,
    /// volume-profile panel collapsed to a thin rail
    pub col_prof: bool,
    /// ORDER BOOK (DOM) panel collapsed to a thin rail
    pub col_dom: bool,
    /// ORDER BOOK mode: true = Compact (tight rows, no chips/staircase)
    pub dom_compact: bool,
    /// header hit-rects filled during draw: [x, y, w, h]
    pub hr_liq: [f64; 4],
    pub hr_prof: [f64; 4],
    pub hr_dom: [f64; 4],
    /// MODE chip inside the ORDER BOOK header (Compact <-> Detailed)
    pub hr_mode: [f64; 4],
    /// mid-price change tracking for the current-price pill timer
    pub last_mid: f64,
    pub mid_since: f64,
    /// stats for the HTML overlay panel (computed during draw)
    pub vol_total: f64,
    pub profit: f64,
    pub px_now: f64,
    pub spr_bps: f64,
    pub imb: f64,
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
            vis_cols: DEF_VIS_COLS,
            intensity: 0.0,
            show_trades: true,
            show_liqs: true,
            show_profile: true,
            show_dom: true,
            show_radar: true,
            show_delta: false,
            show_vwap: true,
            show_candles: true,
            show_volume: true,
            show_trails: true,
            col_liq: false,
            col_prof: false,
            col_dom: false,
            dom_compact: false,
            hr_liq: [0.0; 4],
            hr_prof: [0.0; 4],
            hr_dom: [0.0; 4],
            hr_mode: [0.0; 4],
            last_mid: 0.0,
            mid_since: 0.0,
            vol_total: 0.0,
            profit: 0.0,
            px_now: 0.0,
            spr_bps: 0.0,
            imb: 0.5,
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
        // 60s: the seed is now the full 6000-col history, so re-parsing it is
        // expensive; live WS columns keep the map current between seeds
        self.coin != coin || js_sys::Date::now() - self.fetched_at > 60_000.0
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
        self.vis_cols = DEF_VIS_COLS;
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

    /// Expand every right rail back to full panels.
    pub fn expand_all(&mut self) {
        self.col_liq = false;
        self.col_prof = false;
        self.col_dom = false;
        self.heat = None;
    }

    /// Handle a click on the terminal: returns true when a panel header (or
    /// the ORDER BOOK mode chip / a collapsed rail) consumed it.
    pub fn click(&mut self, x: f64, y: f64) -> bool {
        let hit = |r: &[f64; 4]| {
            r[2] > 0.0 && x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3]
        };
        if hit(&self.hr_mode) {
            self.dom_compact = !self.dom_compact;
            return true;
        }
        if hit(&self.hr_dom) {
            self.col_dom = !self.col_dom;
            self.heat = None; // plot width changes -> full rebuild
            return true;
        }
        if hit(&self.hr_liq) {
            self.col_liq = !self.col_liq;
            self.heat = None;
            return true;
        }
        if hit(&self.hr_prof) {
            self.col_prof = !self.col_prof;
            self.heat = None;
            return true;
        }
        false
    }

    /// True when (x, y) sits on a collapsible panel header (for the cursor).
    pub fn header_hit(&self, x: f64, y: f64) -> bool {
        let hit = |r: &[f64; 4]| {
            r[2] > 0.0 && x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3]
        };
        hit(&self.hr_dom) || hit(&self.hr_liq) || hit(&self.hr_prof) || hit(&self.hr_mode)
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

/// TapeSurf-style collapsible panel header band: drag-grip dots, bold title,
/// chevron pointing down (expanded) or right (collapsed). Returns the chevron
/// x so callers can draw extra controls to its left.
fn panel_header(
    ctx: &CanvasRenderingContext2d,
    x: f64,
    y: f64,
    w: f64,
    title: &str,
    expanded: bool,
    hovered: bool,
) {
    ctx.set_fill_style_str(if hovered { "#13233a" } else { HDR_BG });
    ctx.fill_rect(x, y, w, HDR_H);
    ctx.set_stroke_style_str(HDR_LN);
    ctx.set_line_width(1.0);
    ctx.begin_path();
    ctx.move_to(x, y + HDR_H + 0.5);
    ctx.line_to(x + w, y + HDR_H + 0.5);
    ctx.stroke();
    // drag grip: two columns of three dots
    ctx.set_fill_style_str("#3b526e");
    for (dx, dy) in [(3.0, 4.0), (3.0, 8.0), (3.0, 12.0), (6.5, 4.0), (6.5, 8.0), (6.5, 12.0)] {
        ctx.begin_path();
        let _ = ctx.arc(x + dx, y + dy, 0.9, 0.0, std::f64::consts::TAU);
        let _ = ctx.fill();
    }
    ctx.set_font(FONT_H);
    ctx.set_fill_style_str("#8fb7d4");
    ctx.set_text_align("left");
    let _ = ctx.fill_text(title, x + 13.0, y + 12.0);
    // chevron
    let cx = x + w - 9.0;
    let cy = y + HDR_H / 2.0;
    ctx.set_stroke_style_str("#6d87a6");
    ctx.set_line_width(1.4);
    ctx.set_line_cap("round");
    ctx.begin_path();
    if expanded {
        ctx.move_to(cx - 3.0, cy - 1.5);
        ctx.line_to(cx, cy + 1.8);
        ctx.line_to(cx + 3.0, cy - 1.5);
    } else {
        ctx.move_to(cx - 1.5, cy - 3.0);
        ctx.line_to(cx + 1.8, cy);
        ctx.line_to(cx - 1.5, cy + 3.0);
    }
    ctx.stroke();
    ctx.set_line_cap("butt");
}

/// Collapsed rail: vertical rotated title + right-pointing chevron. The whole
/// rail is the click target (rect returned via `rect`).
fn panel_rail(
    ctx: &CanvasRenderingContext2d,
    x: f64,
    y: f64,
    h: f64,
    title: &str,
    hovered: bool,
) {
    ctx.set_fill_style_str(if hovered { "#13233a" } else { HDR_BG });
    ctx.fill_rect(x, y, RAIL_W, h);
    ctx.set_stroke_style_str(HDR_LN);
    ctx.set_line_width(1.0);
    ctx.stroke_rect(x + 0.5, y + 0.5, RAIL_W - 1.0, h - 1.0);
    // rotated title, reading bottom -> top
    ctx.save();
    ctx.translate(x + RAIL_W / 2.0 + 3.0, y + h / 2.0);
    ctx.rotate(-std::f64::consts::FRAC_PI_2);
    ctx.set_font(FONT_H);
    ctx.set_fill_style_str("#8fb7d4");
    ctx.set_text_align("center");
    let _ = ctx.fill_text(title, 0.0, 0.0);
    ctx.restore();
    ctx.set_text_align("left");
    // chevron at the top of the rail
    let cx = x + RAIL_W / 2.0;
    let cy = y + 10.0;
    ctx.set_stroke_style_str("#6d87a6");
    ctx.set_line_width(1.4);
    ctx.set_line_cap("round");
    ctx.begin_path();
    ctx.move_to(cx - 1.5, cy - 3.0);
    ctx.line_to(cx + 1.8, cy);
    ctx.line_to(cx - 1.5, cy + 3.0);
    ctx.stroke();
    ctx.set_line_cap("butt");
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
    let top_h = 8.0;
    let time_h = 32.0;
    let delta_h = if bv.show_delta { 54.0 } else { 0.0 };
    let axis_w = 70.0;
    let liq_w = if bv.show_radar {
        if bv.col_liq {
            RAIL_W
        } else {
            102.0
        }
    } else {
        0.0
    };
    let prof_w = if bv.show_profile {
        if bv.col_prof {
            RAIL_W
        } else {
            98.0
        }
    } else {
        0.0
    };
    let dom_w = if bv.show_dom {
        if bv.col_dom {
            RAIL_W
        } else if bv.dom_compact {
            (cw * 0.17).clamp(186.0, 250.0)
        } else {
            (cw * 0.22).clamp(232.0, 336.0)
        }
    } else {
        0.0
    };
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

    // ----- collapsible panel hit-rects (headers when open, whole rail when
    // folded) + the MODE chip inside the ORDER BOOK header -----------------
    let hov = |r: &[f64; 4]| {
        bv.hover
            .map(|(x, y)| r[2] > 0.0 && x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3])
            .unwrap_or(false)
    };
    bv.hr_liq = if bv.show_radar {
        if bv.col_liq {
            [lay.liq_x, lay.y, RAIL_W, lay.h]
        } else {
            [lay.liq_x, lay.y, lay.liq_w, HDR_H]
        }
    } else {
        [0.0; 4]
    };
    bv.hr_prof = if bv.show_profile {
        if bv.col_prof {
            [lay.prof_x, lay.y, RAIL_W, lay.h]
        } else {
            [lay.prof_x, lay.y, lay.prof_w, HDR_H]
        }
    } else {
        [0.0; 4]
    };
    bv.hr_dom = if bv.show_dom {
        if bv.col_dom {
            [lay.dom_x, lay.y, RAIL_W, lay.h]
        } else {
            [lay.dom_x, lay.y, lay.dom_w, HDR_H]
        }
    } else {
        [0.0; 4]
    };
    // MODE chip sits at the right of the ORDER BOOK header, left of chevron
    bv.hr_mode = if bv.show_dom && !bv.col_dom {
        [lay.dom_x + lay.dom_w - 78.0, lay.y + 2.0, 58.0, HDR_H - 4.0]
    } else {
        [0.0; 4]
    };
    let hov_liq = hov(&bv.hr_liq);
    let hov_prof = hov(&bv.hr_prof);
    let hov_dom = hov(&bv.hr_dom);
    let hov_mode = hov(&bv.hr_mode);

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
                wlo = pick(HEAT_PLO);
                whi = pick(HEAT_PHI).max(wlo * HEAT_MIN_RATIO);
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

    // ----- candles + volume (TapeSurf-style) ---------------------------------
    let span_ms = vis_n as f64 * 400.0;
    let candle_ms: i64 = if span_ms <= 8.0 * 60_000.0 {
        60_000
    } else if span_ms <= 25.0 * 60_000.0 {
        120_000
    } else {
        300_000
    };
    let cnds = build_candles(&trades_all, first_t, candle_ms);
    let cnds_1m = build_candles(&trades_all, first_t, 60_000);
    bv.vol_total = cnds.iter().map(|c| c.buy + c.sell).sum();
    bv.profit = trades_all
        .iter()
        .filter(|t| t.t >= first_t)
        .map(|t| t.rpn)
        .sum();
    if let Some(b) = bv.cur.as_ref() {
        bv.px_now = b.mid();
        bv.spr_bps = b.spread_bps();
        let (b1, a1) = book_totals(b, 0.01);
        let tot = b1 + a1;
        bv.imb = if tot > 0.0 { b1 / tot } else { 0.5 };
        // mid-change timer for the ORDER BOOK current-price pill
        let m = b.mid();
        if bv.last_mid == 0.0 {
            bv.last_mid = m;
            bv.mid_since = js_sys::Date::now();
        } else if m != bv.last_mid {
            bv.last_mid = m;
            bv.mid_since = js_sys::Date::now();
        }
    }
    // cyan execution-flash columns behind everything
    draw_flashes(&ctx, &lay, &cnds, &t2x, candle_ms, col_w);
    // dotted wall tracks (biggest resting walls per column)
    if bv.show_trails {
        draw_wall_tracks(&ctx, &bv.cols, &lay, skip, vis_n, col_w, &p2y);
    }

    // ----- session VWAP (thin white) ----------------------------------------
    let mut vwap_tag: Option<(f64, f64)> = None;
    if bv.show_vwap {
        vwap_tag = draw_vwap(&ctx, &lay, &trades_all, first_t, &p2y, &t2x);
    }

    // ----- radar magnet levels ----------------------------------------------
    if bv.show_radar {
        if let Some(rs) = radar {
            draw_radar(&ctx, &lay, rs, px_lo, px_hi, &p2y);
        }
    }

    // ----- stepped traded-price path (TapeSurf yellow) ------------------------
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
            // dark casing under the yellow steps so it reads over bright heat
            let step = |col: &str, wd: f64| {
                ctx.set_line_join("miter");
                ctx.set_line_cap("butt");
                ctx.set_stroke_style_str(col);
                ctx.set_line_width(wd);
                ctx.begin_path();
                let (x0, mut cy) = pts[0];
                ctx.move_to(x0, cy);
                for &(x, y) in pts.iter().skip(1) {
                    ctx.line_to(x, cy);
                    ctx.line_to(x, y);
                    cy = y;
                }
                ctx.stroke();
            };
            step("rgba(6,10,18,0.78)", 4.2);
            step(YELLOW, 2.0);
        }
    }

    // ----- candles on top of the path ---------------------------------------
    if bv.show_candles {
        draw_candles(&ctx, &lay, &cnds, &p2y, &t2x, candle_ms, col_w);
    }

    // ----- right-edge pills (TapeSurf price tags) ---------------------------
    let price_pill_y: Option<f64> = if last_px_seen > 0.0 {
        let y = p2y(last_px_seen).clamp(lay.y + 9.0, lay.y + lay.h - 9.0);
        let up = trades_all
            .iter()
            .rev()
            .find(|t| t.t >= first_t)
            .map(|t| t.side.is_buy())
            .unwrap_or(true);
        let txt = format!("{} {}", bv.coin, fmt_comma(last_px_seen));
        let w = (txt.len() as f64 * 5.6 + 10.0).min(lay.axis_w);
        let px = lay.axis_x + 1.0;
        rounded_rect(&ctx, px, y - 8.0, w, 16.0, 4.0, if up { BUY } else { SELL });
        // yellow underline, TapeSurf-style
        ctx.set_fill_style_str(YELLOW);
        ctx.fill_rect(px + 1.0, y + 8.0, w - 2.0, 1.6);
        ctx.set_fill_style_str("#04150d");
        ctx.set_font(FONT_P);
        ctx.set_text_align("center");
        let _ = ctx.fill_text(&txt, px + w / 2.0, y + 3.0);
        ctx.set_text_align("left");
        Some(y)
    } else {
        None
    };
    if let Some((vy, vval)) = vwap_tag {
        if vy > lay.y + 10.0 && vy < lay.y + lay.h - 10.0 {
            // de-conflict with the price pill: shift by ±22px when stacked
            let mut py = vy;
            if let Some(pry) = price_pill_y {
                if (vy - pry).abs() < 20.0 {
                    py = if pry - 22.0 > lay.y + 10.0 {
                        pry - 22.0
                    } else {
                        pry + 22.0
                    };
                }
            }
            let txt = fmt_comma(vval);
            let w = (txt.len() as f64 * 5.6 + 9.0).min(lay.axis_w);
            let px = lay.axis_x + 1.0;
            rounded_rect(&ctx, px, py - 7.0, w, 14.0, 4.0, "#2a3644");
            ctx.set_fill_style_str("#d7e2ec");
            ctx.set_font(FONT_P);
            ctx.set_text_align("center");
            let _ = ctx.fill_text(&txt, px + w / 2.0, py + 3.0);
            ctx.set_text_align("left");
            // thin leader from the pill back to the true VWAP row
            if (py - vy).abs() > 2.0 {
                ctx.set_stroke_style_str("rgba(205,216,226,0.4)");
                ctx.set_line_width(1.0);
                ctx.begin_path();
                ctx.move_to(px + 2.0, py);
                ctx.line_to(px + 2.0, vy);
                ctx.stroke();
            }
        }
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

    // ----- volume histogram strip (TapeSurf bottom bars) ----------------------
    if bv.show_volume {
        // per-minute bars regardless of the candle timeframe: the reference
        // keeps a dense volume tape even when candles widen
        draw_volume_strip(&ctx, &lay, &cnds_1m, &t2x, col_w);
    }

    // ----- volume profile -----------------------------------------------------
    if bv.show_profile && lay.prof_w > 0.0 {
        if bv.col_prof {
            panel_rail(&ctx, lay.prof_x, lay.y, lay.h, "PROFILE", hov_prof);
        } else if lay.prof_w > 40.0 {
            draw_profile(&ctx, &lay, &trades_all, first_t, px_lo, px_hi, &p2y, hov_prof);
        }
    }

    // ----- LIQ LEVELS panel (green long / red short liquidation levels) ------
    if bv.show_radar && lay.liq_w > 0.0 {
        if bv.col_liq {
            panel_rail(&ctx, lay.liq_x, lay.y, lay.h, "LIQ LEVELS", hov_liq);
        } else if lay.liq_w > 40.0 {
            if let Some(rs) = radar {
                if rs.coin == bv.coin {
                    draw_liq_levels(&ctx, &lay, rs, hov_liq);
                }
            }
        }
    }

    // ----- DOM ladder + depth --------------------------------------------------
    if bv.show_dom && lay.dom_w > 0.0 {
        if bv.col_dom {
            panel_rail(&ctx, lay.dom_x, lay.y, lay.h, "ORDER BOOK", hov_dom);
        } else if lay.dom_w > 120.0 {
            if let Some(book) = bv.cur.as_ref() {
                draw_dom(
                    &ctx,
                    &lay,
                    book,
                    bv.dom_compact,
                    hov_dom,
                    hov_mode,
                    bv.mid_since,
                );
                draw_depth(&ctx, &lay, book);
            }
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

    // ----- live-volume chip (TapeSurf-style, right end of the time tape) ------
    {
        let pulse = 0.5 + 0.5 * ((js_sys::Date::now() / 700.0).sin());
        let label = format!("Vol {} · live", fmt_k(bv.vol_total));
        let tw = label.len() as f64 * 5.4;
        let cx = (lay.x + lay.w - tw - 16.0).max(lay.x + 40.0);
        let cy = lay.time_y + 17.0;
        ctx.set_fill_style_str(&format!("rgba(52,240,160,{:.2})", 0.35 + 0.6 * pulse));
        ctx.begin_path();
        let _ = ctx.arc(cx, cy, 3.0, 0.0, std::f64::consts::TAU);
        let _ = ctx.fill();
        ctx.set_fill_style_str("rgba(205,225,240,0.85)");
        ctx.set_font("9px ui-monospace, SFMono-Regular, Menlo, monospace");
        ctx.set_text_align("left");
        let _ = ctx.fill_text(&label, cx + 7.0, cy + 3.0);
    }

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
    for (is_bid, side) in [(true, &col.bids), (false, &col.asks)] {
        if side.len() < 2 {
            continue;
        }
        for j in 0..side.len() {
            let l = side[j];
            let usd = l.px * l.sz;
            let v = ((usd.ln() - lg_lo) / lg_span).clamp(0.0, 1.0);
            if v <= 0.02 {
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
            ctx.set_fill_style_str(if is_bid {
                ramp_bid(v)
            } else {
                ramp_ask(v)
            });
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
    // time axis — labels on the column grid (uniform spacing):
    // "Xh Ym ago" + local clock time underneath (TapeSurf-style tape)
    let step_cols = ((86.0 / col_w).ceil() as usize).max(1);
    ctx.set_font(FONT);
    let now_ms = js_sys::Date::now() as i64;
    let mut i = 0usize;
    while i < vis_n {
        let c = &cols[skip + i];
        let x = lay.w - (vis_n - i) as f64 * col_w;
        if x > lay.x + 34.0 && x < lay.x + lay.w - 118.0 {
            ctx.set_stroke_style_str(GRID);
            ctx.begin_path();
            ctx.move_to(x, lay.y);
            ctx.line_to(x, lay.y + lay.h);
            ctx.stroke();
            ctx.set_fill_style_str(C_DIM);
            ctx.set_text_align("center");
            let _ = ctx.fill_text(&fmt_ago(now_ms - c.t), x, lay.time_y + 11.0);
            ctx.set_fill_style_str("#46586c");
            let _ = ctx.fill_text(&clock12(c.t), x, lay.time_y + 24.0);
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

/// Session VWAP with ±1σ bands (thin white line, like the reference
/// terminal): running USD-weighted mean and variance over the visible
/// prints. Returns `(y, vwap)` at the right edge for the price pill.
fn draw_vwap(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    trades: &[&LiveTrade],
    first_t: i64,
    p2y: &dyn Fn(f64) -> f64,
    t2x: &dyn Fn(i64) -> f64,
) -> Option<(f64, f64)> {
    let _ = lay;
    let mut sum_w = 0.0_f64;
    let mut sum_wp = 0.0_f64;
    let mut sum_wp2 = 0.0_f64;
    // (x, vwap_y, +σ_y, -σ_y)
    let mut band: Vec<(f64, f64, f64, f64, f64)> = Vec::new();
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
            band.push((x, p2y(vwap), p2y(vwap + sig), p2y(vwap - sig), vwap));
            last_x = x;
        }
    }
    if band.len() < 2 {
        return None;
    }
    // band fill
    ctx.set_fill_style_str("rgba(205,216,226,0.07)");
    ctx.begin_path();
    let (x0, _, up0, _, _) = band[0];
    ctx.move_to(x0, up0);
    for (x, _, up, _, _) in band.iter() {
        ctx.line_to(*x, *up);
    }
    for (x, _, _, dn, _) in band.iter().rev() {
        ctx.line_to(*x, *dn);
    }
    ctx.close_path();
    let _ = ctx.fill();
    // vwap line
    ctx.set_stroke_style_str(VWAP);
    ctx.set_line_width(1.0);
    dash(ctx, true, 5.0, 4.0);
    ctx.begin_path();
    let (x0, v0, _, _, _) = band[0];
    ctx.move_to(x0, v0);
    for (x, v, _, _, _) in band.iter() {
        ctx.line_to(*x, *v);
    }
    ctx.stroke();
    dash(ctx, false, 0.0, 0.0);
    // right-edge label
    let (lx, _, lv, _, lval) = *band.last().unwrap();
    ctx.set_font(FONT_B);
    ctx.set_fill_style_str(VWAP);
    ctx.set_text_align("right");
    let _ = ctx.fill_text("VWAP", lx - 3.0, lv - 4.0);
    ctx.set_text_align("left");
    ctx.set_font(FONT);
    Some((lv, lval))
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
/// with LEVEL MAX / CUM MAX stats and reference-style price pills on the
/// biggest armed levels (solid price pill + "×hits" tag), like the
/// screenshots. The panel maps its own wider price domain (levels within
/// ±8% of the last price) so the full liquidation landscape stays visible
/// even when the chart itself is zoomed tight around the traded range.
fn draw_liq_levels(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    rs: &shared::RadarSnapshot,
    hovered: bool,
) {
    let x0 = lay.liq_x + 3.0;
    let w_in = lay.liq_w - 6.0;
    // collapsible header band
    panel_header(ctx, lay.liq_x, lay.y, lay.liq_w, "LIQ LEVELS", true, hovered);
    ctx.set_fill_style_str("rgba(4,10,20,0.72)");
    ctx.fill_rect(lay.liq_x, lay.y + HDR_H, lay.liq_w, lay.h - HDR_H);

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
    let hdr_h = 64.0;
    let body_y = lay.y + HDR_H + hdr_h;
    let body_h = (lay.h - HDR_H - hdr_h - 14.0).max(20.0);
    let rows = ((body_h / 3.0) as usize).clamp(16, 220);
    let row_h = body_h / rows as f64;
    let mut long_rows = vec![0.0_f64; rows];
    let mut short_rows = vec![0.0_f64; rows];
    // strongest armed level per side, for the price pills
    let mut big_long: Option<&shared::RadarLevel> = None;
    let mut big_short: Option<&shared::RadarLevel> = None;
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
        if lv.state != LevelState::Swept {
            let take = |cur: &Option<&shared::RadarLevel>| {
                cur.map_or(true, |c| lv.est_usd > c.est_usd)
            };
            if lv.long && take(&big_long) {
                big_long = Some(lv);
            } else if !lv.long && take(&big_short) {
                big_short = Some(lv);
            }
        }
    }
    let max_l = long_rows.iter().cloned().fold(0.0_f64, f64::max);
    let max_s = short_rows.iter().cloned().fold(0.0_f64, f64::max);
    let cum_l: f64 = long_rows.iter().sum();
    let cum_s: f64 = short_rows.iter().sum();

    // stats: two lines per side (LEVEL MAX + CUM MAX), green then red
    let sy = lay.y + HDR_H;
    ctx.set_font("8px ui-monospace, SFMono-Regular, Menlo, monospace");
    ctx.set_fill_style_str("#34f0a0");
    let _ = ctx.fill_text(&format!("LEVEL MAX {}", fmt_k(max_l)), x0, sy + 11.0);
    let _ = ctx.fill_text(&format!("CUM MAX {}", fmt_k(cum_l)), x0, sy + 20.0);
    ctx.set_fill_style_str("#ff5c72");
    let _ = ctx.fill_text(&format!("LEVEL MAX {}", fmt_k(max_s)), x0, sy + 31.0);
    let _ = ctx.fill_text(&format!("CUM MAX {}", fmt_k(cum_s)), x0, sy + 40.0);
    // side markers + header separator
    ctx.set_fill_style_str("rgba(46,224,150,0.9)");
    ctx.fill_rect(x0, sy + 46.0, 5.0, 3.0);
    ctx.set_fill_style_str("rgba(255,77,96,0.9)");
    ctx.fill_rect(x0 + w_in - 5.0, sy + 46.0, 5.0, 3.0);
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

    // reference-style price pills on the biggest armed levels:
    // solid side-coloured price pill + dark "×hits" tag beneath
    let pill = |lv: &shared::RadarLevel, col: &str| {
        let y = body_y + ((dom_hi - lv.px) / span * body_h).clamp(8.0, body_h - 20.0);
        let txt = fmt_comma(lv.px);
        let pw = (txt.len() as f64 * 5.8 + 8.0).min(w_in);
        let py = y - 16.0;
        ctx.set_font(FONT_P);
        rounded_rect(ctx, x0 + w_in - pw, py, pw, 13.0, 3.0, col);
        ctx.set_fill_style_str("#0a0d12");
        ctx.set_text_align("center");
        let _ = ctx.fill_text(&txt, x0 + w_in - pw / 2.0, py + 9.5);
        // ×hits tag (like the reference "B1x87")
        let tag = format!("×{}", lv.hits.max(1));
        let tw = tag.len() as f64 * 5.4 + 6.0;
        rounded_rect(ctx, x0 + w_in - tw, py + 14.0, tw, 11.0, 3.0, "#101a26");
        ctx.set_fill_style_str(col);
        let _ = ctx.fill_text(&tag, x0 + w_in - tw / 2.0, py + 22.5);
        ctx.set_text_align("left");
        ctx.set_font(FONT);
    };
    if let Some(lv) = big_long.filter(|lv| lv.est_usd > 50_000.0) {
        pill(lv, "rgba(34,196,120,0.95)");
    }
    if let Some(lv) = big_short.filter(|lv| lv.est_usd > 50_000.0) {
        pill(lv, "rgba(228,52,80,0.95)");
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
    hovered: bool,
) {
    // collapsible header band
    panel_header(ctx, lay.prof_x, lay.y, lay.prof_w, "PROFILE", true, hovered);
    let top = lay.y + HDR_H;
    let body_h = lay.h - HDR_H;
    let rows = ((body_h / 3.0) as usize).clamp(24, 260);
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
    let traded: f64 = buy.iter().zip(sell.iter()).map(|(b, s)| b + s).sum();
    let delta: f64 = buy.iter().zip(sell.iter()).map(|(b, s)| b - s).sum();
    let row_max = buy
        .iter()
        .zip(sell.iter())
        .map(|(b, s)| b + s)
        .fold(0.0_f64, f64::max);
    // TapeSurf-style stat header: TRADED / ROW MAX / Δ (+%)
    ctx.set_font("8px ui-monospace, SFMono-Regular, Menlo, monospace");
    let x0 = lay.prof_x + 3.0;
    ctx.set_fill_style_str("#8fb7d4");
    let _ = ctx.fill_text(&format!("TRADED {}", fmt_k(traded)), x0, top + 10.0);
    let _ = ctx.fill_text(&format!("ROW MAX {}", fmt_k(row_max)), x0, top + 19.0);
    let pct = if traded > 0.0 {
        delta / traded * 100.0
    } else {
        0.0
    };
    ctx.set_fill_style_str(if delta >= 0.0 { "#34f0a0" } else { "#ff5c72" });
    let _ = ctx.fill_text(
        &format!("Δ {}{} · {:.1}%", if delta >= 0.0 { "+" } else { "-" }, fmt_k(delta.abs()), pct),
        x0,
        top + 28.0,
    );
    let max_side = buy
        .iter()
        .chain(sell.iter())
        .cloned()
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let mid_x = lay.prof_x + lay.prof_w * 0.5;
    let row_h = body_h / rows as f64;
    for r in 0..rows {
        let y = top + r as f64 * row_h;
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
    let total: f64 = traded;
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
            ty = ty.clamp(top + 9.0, lay.y + lay.h - 3.0);
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

/// TapeSurf-style ORDER BOOK panel — the live limit-order ladder.
/// Detailed mode: direction glyphs, price column, depth bars with the USD
/// size inside, order-count chips (Hyperliquid reports the aggregated order
/// count per level, the on-chain equivalent of the reference's venue
/// chips), a blue cumulative-depth staircase along the right edge, a
/// bids/asks % split header, MAX stat, bottom $-scale labels and a
/// highlighted current-price pill with a time-at-price timer.
/// Compact mode: tight rows, no chips / staircase / scale.
fn draw_dom(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    book: &BookCol,
    compact: bool,
    hovered: bool,
    hovered_mode: bool,
    mid_since: f64,
) {
    let x0 = lay.dom_x;
    let w = lay.dom_w;
    // ----- header band + MODE chip ----------------------------------------
    panel_header(ctx, x0, lay.y, w, "ORDER BOOK", true, hovered);
    let mode_txt = if compact { "COMPACT" } else { "DETAILED" };
    let mw = 58.0;
    let mx = x0 + w - 14.0 - mw;
    rounded_rect(
        ctx,
        mx,
        lay.y + 2.5,
        mw,
        HDR_H - 5.0,
        3.0,
        if hovered_mode { "#1b3250" } else { "#101d2e" },
    );
    ctx.set_font(FONT_S);
    ctx.set_fill_style_str("#9fc0dd");
    ctx.set_text_align("center");
    let _ = ctx.fill_text(mode_txt, mx + mw / 2.0, lay.y + 12.0);
    ctx.set_text_align("left");
    // GROUP hint left of the MODE chip
    ctx.set_fill_style_str("#4c617a");
    let _ = ctx.fill_text("GROUP auto", x0 + 64.0, lay.y + 12.0);
    ctx.set_font(FONT);

    let top = lay.y + HDR_H;
    // ----- column header row: PRICE | LEVEL USD | MAX ----------------------
    let (b1, a1) = book_totals(book, 0.01);
    let tot = b1 + a1;
    let asks: Vec<&shared::BookLevel> = book.asks.iter().take(34).collect();
    let bids: Vec<&shared::BookLevel> = book.bids.iter().take(34).collect();
    let max_usd = asks
        .iter()
        .chain(bids.iter())
        .map(|l| l.px * l.sz)
        .fold(1.0_f64, f64::max);

    let px_col_w = 62.0;
    let glyph_w = 9.0;
    let bar_x = x0 + glyph_w + px_col_w + 6.0;
    let stair_w = if compact { 0.0 } else { 40.0 };
    let bar_w = (w - (bar_x - x0) - stair_w - 8.0).max(20.0);

    ctx.set_font(FONT_S);
    ctx.set_fill_style_str("#5b6b80");
    let _ = ctx.fill_text("PRICE", x0 + glyph_w + 2.0, top + 9.0);
    ctx.set_text_align("right");
    let _ = ctx.fill_text("LEVEL USD", bar_x + bar_w.min(70.0), top + 9.0);
    if !compact {
        ctx.set_fill_style_str("#4c617a");
        let _ = ctx.fill_text(&format!("MAX {}", fmt_k(max_usd)), x0 + w - 6.0, top + 9.0);
    }
    ctx.set_text_align("left");

    // ----- bids / asks % split bar -----------------------------------------
    let imb = if tot > 0.0 { b1 / tot } else { 0.5 };
    let split_y = top + 12.0;
    let bp = (imb * 100.0).round();
    ctx.set_fill_style_str("rgba(31,200,150,0.85)");
    ctx.fill_rect(x0 + 3.0, split_y, (w - 6.0) * imb, 10.0);
    ctx.set_fill_style_str("rgba(229,72,77,0.85)");
    ctx.fill_rect(x0 + 3.0 + (w - 6.0) * imb, split_y, (w - 6.0) * (1.0 - imb), 10.0);
    ctx.set_font(FONT_S);
    ctx.set_fill_style_str("#04150d");
    if (w - 6.0) * imb > 62.0 {
        let _ = ctx.fill_text(&format!("bids {:.0}%", bp), x0 + 6.0, split_y + 7.5);
    }
    if (w - 6.0) * (1.0 - imb) > 58.0 {
        ctx.set_text_align("right");
        let _ = ctx.fill_text(&format!("{:.0}% asks", 100.0 - bp), x0 + w - 6.0, split_y + 7.5);
        ctx.set_text_align("left");
    }

    // ----- ladder rows ------------------------------------------------------
    let row_h = if compact { 13.0 } else { 15.0 };
    let ladder_top = split_y + 14.0;
    // reserve room for the depth chart pinned to the panel bottom
    let depth_room = if lay.h - (split_y + 14.0 - lay.y) >= 242.0 { 96.0 } else { 8.0 };
    let ladder_h = lay.h - (ladder_top - lay.y) - depth_room;
    let spread_h = 22.0;
    let rows_per_side = (((ladder_h - spread_h) * 0.5 / row_h) as usize)
        .clamp(4, 30)
        .min(asks.len())
        .min(bids.len());
    let asks: Vec<&shared::BookLevel> = asks.into_iter().take(rows_per_side).collect();
    let bids: Vec<&shared::BookLevel> = bids.into_iter().take(rows_per_side).collect();
    // cumulative depth measured from the spread outward (staircase source):
    // prefix[k] = sum of the best k levels of the side
    let prefix = |side: &[&shared::BookLevel]| -> Vec<f64> {
        let mut v = Vec::with_capacity(side.len() + 1);
        let mut c = 0.0_f64;
        v.push(0.0);
        for l in side.iter() {
            c += l.px * l.sz;
            v.push(c);
        }
        v
    };
    let ask_pre = prefix(&asks);
    let bid_pre = prefix(&bids);

    // one row painter; returns the (y, usd) for the staircase
    let mut row = |l: &shared::BookLevel, is_bid: bool, y: f64| -> (f64, f64) {
        let usd = l.px * l.sz;
        let bw = (usd / max_usd * bar_w).max(1.0);
        let (bar_col, hot_col, px_col) = if is_bid {
            ("rgba(31,200,150,0.72)", "#4ef0be", "#67e8c3")
        } else {
            ("rgba(229,72,77,0.72)", "#ff8f93", "#ff9b9f")
        };
        // depth bar
        ctx.set_fill_style_str(bar_col);
        ctx.fill_rect(bar_x, y, bw, row_h - 2.0);
        if usd >= max_usd * 0.999 {
            ctx.set_stroke_style_str(hot_col);
            ctx.set_line_width(1.0);
            ctx.stroke_rect(bar_x, y, bw, row_h - 2.0);
        }
        // direction glyph (asks point up away from mid, bids down)
        ctx.set_fill_style_str(if is_bid { "#2f9e7c" } else { "#b04a55" });
        ctx.set_font("8px ui-monospace, SFMono-Regular, Menlo, monospace");
        let _ = ctx.fill_text(if is_bid { "▼" } else { "▲" }, x0 + 2.0, y + row_h - 5.0);
        // price
        ctx.set_fill_style_str(px_col);
        ctx.set_text_align("right");
        let _ = ctx.fill_text(&fmt_px(l.px), x0 + glyph_w + px_col_w, y + row_h - 5.0);
        ctx.set_text_align("left");
        // USD size: inside the bar when it fits, else just past its tip
        let sz_txt = fmt_k(usd);
        ctx.set_font(FONT);
        let fits = bw > sz_txt.len() as f64 * 6.2 + 6.0;
        if fits {
            ctx.set_fill_style_str("#0a0f16");
            let _ = ctx.fill_text(&sz_txt, bar_x + 4.0, y + row_h - 5.0);
        } else {
            ctx.set_fill_style_str(C_TXT);
            let _ = ctx.fill_text(&sz_txt, bar_x + bw + 4.0, y + row_h - 5.0);
        }
        // order-count chip (limit orders aggregated at this price)
        if !compact && l.n >= 2.0 {
            let chip = format!("×{}", l.n as u32);
            let cw2 = chip.len() as f64 * 5.6 + 7.0;
            let cx = bar_x + bw + (if fits { sz_txt.len() as f64 * 6.2 + 8.0 } else { 0.0 });
            if cx + cw2 < bar_x + bar_w + stair_w {
                rounded_rect(ctx, cx, y + 1.5, cw2, row_h - 5.0, 3.0, "#0f1a28");
                ctx.set_fill_style_str("#8fb7d4");
                ctx.set_font(FONT_S);
                let _ = ctx.fill_text(&chip, cx + 3.5, y + row_h - 5.5);
                ctx.set_font(FONT);
            }
        }
        (y + row_h / 2.0, usd)
    };

    // asks: worst at top, best adjacent to the spread.
    // staircase point per row: cumulative depth from the spread outward
    let mut y = ladder_top;
    let mut stair: Vec<(f64, f64)> = Vec::new(); // (row center y, cum-from-mid usd)
    let n_ask = asks.len();
    for (i, l) in asks.iter().rev().enumerate() {
        let _ = row(l, false, y);
        // drawn row i (worst-first) is asks[n_ask-1-i]: cum = ask_pre[n_ask-i]
        stair.push((y + row_h / 2.0, ask_pre[n_ask - i]));
        y += row_h;
    }
    // spread / current-price row (teal pill + time-at-price timer)
    let best_bid = book.bids.first().map(|l| l.px).unwrap_or(0.0);
    let best_ask = book.asks.first().map(|l| l.px).unwrap_or(0.0);
    let mid = (best_bid + best_ask) / 2.0;
    let since_s = ((js_sys::Date::now() - mid_since).max(0.0) / 1000.0) as i64;
    let timer = format!("{}:{:02}", since_s / 60, since_s % 60);
    ctx.set_fill_style_str("rgba(20,32,46,0.95)");
    ctx.fill_rect(x0 + 2.0, y, w - 6.0, spread_h);
    let pill_txt = fmt_px(mid);
    let pw = pill_txt.len() as f64 * 6.2 + 12.0;
    rounded_rect(ctx, x0 + glyph_w + 2.0, y + 2.0, pw, 13.0, 3.0, "#0e7f8f");
    ctx.set_fill_style_str("#eafcff");
    ctx.set_font(FONT_P);
    ctx.set_text_align("center");
    let _ = ctx.fill_text(&pill_txt, x0 + glyph_w + 2.0 + pw / 2.0, y + 11.5);
    ctx.set_text_align("left");
    ctx.set_font(FONT_S);
    ctx.set_fill_style_str("#7dd3fc");
    let _ = ctx.fill_text(
        &format!("Δ{:.1}bp", book.spread_bps()),
        x0 + glyph_w + pw + 10.0,
        y + 11.5,
    );
    let _ = ctx.fill_text(&timer, x0 + w - 34.0, y + 11.5);
    ctx.set_fill_style_str("#4c617a");
    let _ = ctx.fill_text("at price", x0 + w - 82.0, y + 11.5);
    y += spread_h + 2.0;
    // bids: best just below the spread (cum-from-mid = bid_pre[i+1])
    for (i, l) in bids.iter().enumerate() {
        let _ = row(l, true, y);
        stair.push((y + row_h / 2.0, bid_pre[i + 1]));
        y += row_h;
    }

    // ----- cumulative-depth staircase (blue, reference style) ---------------
    // both sides grow away from the spread row: x = sx + (cum/stair_max) * sw
    if !compact && stair.len() >= 2 {
        let stair_max = ask_pre
            .last()
            .copied()
            .unwrap_or(1.0)
            .max(bid_pre.last().copied().unwrap_or(1.0))
            .max(1.0);
        ctx.set_stroke_style_str("#3d8fd9");
        ctx.set_line_width(1.2);
        ctx.begin_path();
        let sx = bar_x + bar_w + 6.0;
        let sw = stair_w - 10.0;
        for (i, (sy, c)) in stair.iter().enumerate() {
            let x = sx + (c / stair_max).min(1.0) * sw;
            if i == 0 {
                ctx.move_to(x, *sy);
            } else {
                ctx.line_to(x, *sy);
            }
        }
        ctx.stroke();
    }

    // ----- bottom $ scale labels --------------------------------------------
    if !compact {
        let sy = ladder_top + ladder_h - 1.0;
        ctx.set_font(FONT_S);
        ctx.set_fill_style_str("#46586c");
        let half = max_usd * 0.5;
        let _ = ctx.fill_text(&format!("${}", fmt_k(half)), bar_x + bar_w * 0.5 - 12.0, sy);
        ctx.set_text_align("right");
        let _ = ctx.fill_text(&format!("${}", fmt_k(max_usd)), bar_x + bar_w, sy);
        ctx.set_text_align("left");
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
// TapeSurf-style overlays: candles, volume bars, execution flashes, wall trails
// ---------------------------------------------------------------------------

/// one aggregated candle bucket (built client-side from the print tape)
struct Cnd {
    t0: i64,
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    buy: f64,
    sell: f64,
}

fn build_candles(trades: &[&LiveTrade], first_t: i64, bucket_ms: i64) -> Vec<Cnd> {
    let mut ts: Vec<&LiveTrade> = trades
        .iter()
        .filter(|t| t.t >= first_t && t.px > 0.0)
        .copied()
        .collect();
    ts.sort_by_key(|t| t.t);
    let mut out: Vec<Cnd> = Vec::new();
    for t in ts {
        let t0 = t.t.div_euclid(bucket_ms) * bucket_ms;
        let (buy, sell) = if t.side.is_buy() {
            (t.usd, 0.0)
        } else {
            (0.0, t.usd)
        };
        match out.last_mut() {
            Some(c) if c.t0 == t0 => {
                c.h = c.h.max(t.px);
                c.l = c.l.min(t.px);
                c.c = t.px;
                c.buy += buy;
                c.sell += sell;
            }
            _ => out.push(Cnd {
                t0,
                o: t.px,
                h: t.px,
                l: t.px,
                c: t.px,
                buy,
                sell,
            }),
        }
    }
    out
}

fn draw_candles(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    cnds: &[Cnd],
    p2y: &dyn Fn(f64) -> f64,
    t2x: &dyn Fn(i64) -> f64,
    candle_ms: i64,
    col_w: f64,
) {
    let candle_cols = (candle_ms / 400).max(1) as f64;
    let body_w = (candle_cols * col_w * 0.62).clamp(2.0, 9.0);
    for c in cnds {
        let xc = t2x(c.t0 + candle_ms / 2);
        if xc < lay.x - 8.0 || xc > lay.x + lay.w + 8.0 {
            continue;
        }
        let yh = p2y(c.h);
        let yl = p2y(c.l);
        let yo = p2y(c.o);
        let yc = p2y(c.c);
        if yh < lay.y - 30.0 || yl > lay.y + lay.h + 30.0 {
            continue;
        }
        let up = c.c >= c.o;
        let col = if up { CANDLE_UP } else { CANDLE_DN };
        // wick
        ctx.set_stroke_style_str(col);
        ctx.set_line_width(1.0);
        ctx.begin_path();
        ctx.move_to(xc, yh);
        ctx.line_to(xc, yl);
        ctx.stroke();
        // body (mostly opaque with a dark rim so it pops off bright heat)
        let by0 = yo.min(yc);
        let bh = (yc - yo).abs().max(1.2);
        ctx.set_fill_style_str(if up {
            "rgba(23,211,161,0.92)"
        } else {
            "rgba(255,77,94,0.92)"
        });
        ctx.fill_rect(xc - body_w / 2.0, by0, body_w, bh);
        ctx.set_stroke_style_str("rgba(4,10,14,0.85)");
        ctx.set_line_width(1.0);
        ctx.stroke_rect(xc - body_w / 2.0, by0, body_w, bh);
        let _ = col;
    }
}

/// thin volume histogram along the bottom of the plot (red / teal bars);
/// buckets are always 1 minute so the tape stays dense at every zoom
fn draw_volume_strip(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    cnds: &[Cnd],
    t2x: &dyn Fn(i64) -> f64,
    col_w: f64,
) {
    if cnds.is_empty() {
        return;
    }
    let strip_h = 34.0_f64;
    let base_y = lay.y + lay.h;
    let candle_cols = (60_000 / 400).max(1) as f64;
    let bar_w = (candle_cols * col_w * 0.62).clamp(1.6, 9.0);
    let max_vol = cnds
        .iter()
        .map(|c| c.buy + c.sell)
        .fold(1e-9_f64, f64::max);
    let max_sq = max_vol.sqrt();
    for c in cnds {
        let xc = t2x(c.t0 + 30_000);
        if xc < lay.x - 8.0 || xc > lay.x + lay.w + 8.0 {
            continue;
        }
        let vol = c.buy + c.sell;
        if vol <= 0.0 {
            continue;
        }
        let bh = (vol.sqrt() / max_sq * strip_h).max(1.0);
        ctx.set_fill_style_str(if c.buy >= c.sell {
            "rgba(23,211,161,0.55)"
        } else {
            "rgba(255,77,94,0.55)"
        });
        ctx.fill_rect(xc - bar_w / 2.0, base_y - bh, bar_w, bh);
    }
    // baseline
    ctx.set_stroke_style_str("rgba(120,140,160,0.25)");
    ctx.set_line_width(1.0);
    ctx.begin_path();
    ctx.move_to(lay.x, base_y - 0.5);
    ctx.line_to(lay.x + lay.w, base_y - 0.5);
    ctx.stroke();
}

/// cyan execution-flash columns behind the overlays (volume bursts)
fn draw_flashes(
    ctx: &CanvasRenderingContext2d,
    lay: &Lay,
    cnds: &[Cnd],
    t2x: &dyn Fn(i64) -> f64,
    candle_ms: i64,
    col_w: f64,
) {
    if cnds.len() < 6 {
        return;
    }
    let mut vols: Vec<f64> = cnds.iter().map(|c| c.buy + c.sell).collect();
    vols.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p85 = vols[((vols.len() as f64 - 1.0) * 0.85) as usize].max(1.0);
    let max_vol = vols.last().copied().unwrap_or(1.0).max(1.0);
    let candle_cols = (candle_ms / 400).max(1) as f64;
    let w = (candle_cols * col_w * 0.9).max(2.0);
    for c in cnds {
        let vol = c.buy + c.sell;
        if vol < p85 {
            continue;
        }
        let xc = t2x(c.t0 + candle_ms / 2);
        if xc < lay.x - w || xc > lay.x + lay.w + w {
            continue;
        }
        let norm = ((vol / max_vol) - 0.6).max(0.0) / 0.4;
        let alpha = 0.05 + 0.11 * norm;
        ctx.set_fill_style_str(&format!("rgba(53,216,230,{:.3})", alpha));
        ctx.fill_rect(xc - w / 2.0, lay.y, w, lay.h);
    }
}

fn max_level(side: &[shared::BookLevel]) -> (f64, f64) {
    let mut best = (0.0_f64, 0.0_f64);
    for l in side {
        let usd = l.px * l.sz;
        if usd > best.0 {
            best = (usd, l.px);
        }
    }
    best
}

/// dotted trails that follow the biggest resting walls across columns
fn draw_wall_tracks(
    ctx: &CanvasRenderingContext2d,
    cols: &VecDeque<BookCol>,
    lay: &Lay,
    skip: usize,
    vis_n: usize,
    col_w: f64,
    p2y: &dyn Fn(f64) -> f64,
) {
    let stride = (vis_n / 420).max(1);
    // sampled (x, bid_max(usd,px), ask_max(usd,px)) per column
    let mut samples: Vec<(f64, (f64, f64), (f64, f64))> = Vec::new();
    let mut i = 0usize;
    while i < vis_n {
        let c = &cols[skip + i];
        let x = lay.w - (vis_n - i) as f64 * col_w;
        samples.push((x, max_level(&c.bids), max_level(&c.asks)));
        i += stride;
    }
    if samples.len() < 12 {
        return;
    }
    // per-side dotted trail: threshold = p92 of the sampled max sizes
    let draw_side = |get: &dyn Fn(&(f64, (f64, f64), (f64, f64))) -> (f64, f64),
                     col: &str| {
        let mut vols: Vec<f64> = samples.iter().map(|s| get(s).0).collect();
        vols.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let thr = vols[((vols.len() as f64 - 1.0) * 0.92) as usize].max(1.0);
        ctx.set_stroke_style_str(col);
        ctx.set_line_width(1.0);
        dash(ctx, true, 1.5, 2.5);
        let mut last: Option<(f64, f64)> = None;
        for s in samples.iter() {
            let (usd, px) = get(s);
            if usd < thr || px <= 0.0 {
                last = None;
                continue;
            }
            let y = p2y(px);
            if y < lay.y - 4.0 || y > lay.y + lay.h + 4.0 {
                last = None;
                continue;
            }
            let x = s.0;
            if let Some((lx, ly)) = last {
                if (ly - y).abs() < 2.5 {
                    ctx.begin_path();
                    ctx.move_to(lx, ly);
                    ctx.line_to(x, y);
                    ctx.stroke();
                }
            }
            ctx.set_fill_style_str(col);
            ctx.begin_path();
            let _ = ctx.arc(x, y, 1.1, 0.0, std::f64::consts::TAU);
            let _ = ctx.fill();
            last = Some((x, y));
        }
        dash(ctx, false, 0.0, 0.0);
    };
    draw_side(&|s: &(f64, (f64, f64), (f64, f64))| s.1, "rgba(126,232,171,0.40)");
    draw_side(&|s: &(f64, (f64, f64), (f64, f64))| s.2, "rgba(126,204,238,0.42)");
}


pub fn wheel(bv: &mut BookView, dy: f64) {
    let f = if dy > 0.0 { 1.12 } else { 1.0 / 1.12 };
    let v = ((bv.vis_cols as f64 * f) as usize).clamp(80, 6_000);
    if v != bv.vis_cols {
        bv.vis_cols = v;
        bv.heat = None;
    }
}
