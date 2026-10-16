//! Imperative DOM panel rendering: tape rows, liquidation feed, trader
//! leaderboard, flow stats, the level drill-down modal, the radar panel,
//! the opportunities feed and the tuning controls.
//! All helpers return HTML strings; the app injects them via innerHTML.

use shared::{
    fmt_qty, fmt_usd, short_addr, CascadeRecord, FootWindow, LevelDetail, LiveLiquidation,
    LiveTrade, Opportunity, RadarLevel, RadarSnapshot, TraderRow, TuningParams,
};

// ---------------------------------------------------------------------------
// number / time formatting
// ---------------------------------------------------------------------------

fn with_commas(s: &str) -> String {
    let (int, rest) = match s.find('.') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let sign = if int.starts_with('-') { "-" } else { "" };
    let digits = int.trim_start_matches('-');
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{sign}{out}{rest}")
}

/// Price with adaptive decimals + thousands separators.
pub fn fmt_px(v: f64) -> String {
    if !v.is_finite() || v <= 0.0 {
        return "—".into();
    }
    let dec = if v >= 10_000.0 {
        1
    } else if v >= 100.0 {
        2
    } else if v >= 1.0 {
        3
    } else {
        5
    };
    with_commas(&format!("{:.*}", dec, v))
}

/// Compact notional for canvas cells (no $ sign to save width).
pub fn fmt_k(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e3 {
        format!("{:.1}K", v / 1e3)
    } else if a >= 1.0 {
        format!("{:.0}", v)
    } else if a > 0.0 {
        format!("{:.2}", v)
    } else {
        "0".into()
    }
}

pub fn ms_hms(ms: i64) -> String {
    let s = (ms / 1000).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

pub fn ms_hm(ms: i64) -> String {
    let s = (ms / 1000).rem_euclid(86_400);
    format!("{:02}:{:02}", s / 3600, (s / 60) % 60)
}

fn tag_class(tag: &str) -> &'static str {
    match tag {
        "++" => "t2",
        "+" => "t1",
        "±" => "t0",
        "-" => "t3",
        "--" => "t4",
        _ => "t0",
    }
}

const SCAN: &str = "https://hypurrscan.io/address/";

// ---------------------------------------------------------------------------
// tape
// ---------------------------------------------------------------------------

pub fn tape_row_html(t: &LiveTrade) -> String {
    let buy = t.side.is_buy();
    let (side_lbl, cls) = if buy { ("BUY", "buy") } else { ("SELL", "sell") };
    let badge = format!(
        "<span class='badge' style='background:{}'>{}</span>",
        t.intent.color(),
        t.intent.label()
    );
    let rpn = if t.rpn.abs() > 0.005 {
        let c = if t.rpn > 0.0 { "up" } else { "down" };
        format!(
            "<span class='rpn {c}'>{}{}</span>",
            if t.rpn > 0.0 { "+" } else { "−" },
            fmt_usd(t.rpn.abs())
        )
    } else {
        "<span class='rpn zero'>·</span>".into()
    };
    let title = format!("{} {} · maker {}", t.taker_style, t.taker_tag, short_addr(&t.maker));
    format!(
        "<div class='trow fresh {cls}'><span class='t'>{}</span><span class='side'>{side_lbl}</span>\
         <span class='px'>{}</span><span class='sz'>{}</span><span class='usd'>{}</span>\
         {badge}{rpn}<a href='{SCAN}{}' target='_blank' title='{title}'>{}</a></div>",
        ms_hms(t.t),
        fmt_px(t.px),
        fmt_qty(t.sz),
        fmt_usd(t.usd),
        t.taker,
        short_addr(&t.taker),
    )
}

// ---------------------------------------------------------------------------
// liquidation feed
// ---------------------------------------------------------------------------

pub fn liq_row_html(l: &LiveLiquidation) -> String {
    let dir = if l.long { "LONG" } else { "SHORT" };
    let cls = if l.long { "long" } else { "short" };
    format!(
        "<div class='liq-row fresh'><span class='t'>{}</span><span class='c'>{}</span>\
         <span class='dir {cls}'>{dir}</span><span class='amt'>{}</span>\
         <a href='{SCAN}{}' target='_blank' title='liquidated @ {}'>{}</a></div>",
        ms_hms(l.t),
        l.coin,
        fmt_usd(l.usd),
        l.user,
        fmt_px(l.px),
        short_addr(&l.user),
    )
}

// ---------------------------------------------------------------------------
// traders leaderboard
// ---------------------------------------------------------------------------

pub fn traders_html(rows: &[TraderRow]) -> String {
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let vol = r.bought + r.sold;
            let bb = if vol > 0.0 { r.bought / vol } else { 0.5 };
            let liq_chip = if r.liq_count > 0 {
                format!("<span class='liqn'>LIQ×{}</span>", r.liq_count)
            } else {
                String::new()
            };
            let pnl = match r.pnl {
                Some(p) => format!(
                    "<span class='pnl {}'>{}</span>",
                    if p >= 0.0 { "up" } else { "down" },
                    fmt_usd(p)
                ),
                None => "<span class='pnl na'>pnl —</span>".into(),
            };
            let wr = match r.win_rate {
                Some(w) => format!("{:.0}% · {}f", w * 100.0, r.trades),
                None => format!("{}f", r.trades),
            };
            format!(
                "<div class='tr-row'><div class='l1'><span class='rank'>{}</span>\
                 <a href='{SCAN}{}' target='_blank'>{}</a>\
                 <span class='sty'>{}</span><span class='tag {}'>{}</span>{liq_chip}\
                 <span class='vol'>{}</span></div>\
                 <div class='l2'><div class='bar'><div class='b' style='width:{:.1}%'></div>\
                 <div class='s' style='width:{:.1}%'></div></div>\
                 <span>share {:.0}%</span>{pnl}<span class='wr'>{wr}</span></div></div>",
                i + 1,
                r.a,
                short_addr(&r.a),
                r.style,
                tag_class(&r.tag),
                r.tag,
                fmt_usd(vol),
                bb * 100.0,
                (1.0 - bb) * 100.0,
                r.share * 100.0,
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// flow stats
// ---------------------------------------------------------------------------

pub struct FlowSums {
    pub buy: f64,
    pub sell: f64,
    pub liq: f64,
    pub stop: f64,
    pub tp: f64,
    pub hedge: f64,
    pub prints: u32,
    pub wallets: usize,
    pub span_min: i64,
}

pub fn flow_sums(windows: &[FootWindow]) -> FlowSums {
    let mut s = FlowSums {
        buy: 0.0,
        sell: 0.0,
        liq: 0.0,
        stop: 0.0,
        tp: 0.0,
        hedge: 0.0,
        prints: 0,
        wallets: 0,
        span_min: 0,
    };
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    let mut wallets = std::collections::HashSet::new();
    for w in windows {
        s.buy += w.buy;
        s.sell += w.sell;
        s.prints += w.prints;
        for lv in &w.levels {
            s.liq += lv.liq;
            s.stop += lv.stop;
            s.tp += lv.tp;
            s.hedge += lv.hedge;
        }
        for t in w.tb.iter().chain(w.ts.iter()) {
            wallets.insert(t.a.clone());
        }
        lo = lo.min(w.t);
        hi = hi.max(w.t);
    }
    s.wallets = wallets.len();
    if lo != i64::MAX {
        s.span_min = (hi - lo) / 60_000 + 1;
    }
    s
}

// ---------------------------------------------------------------------------
// level drill-down modal
// ---------------------------------------------------------------------------

pub fn level_modal_html(d: &LevelDetail) -> String {
    let tile = |k: &str, v: String, cls: &str| {
        format!("<div class='m-tile'><div class='k'>{k}</div><div class='v {cls}'>{v}</div></div>")
    };
    let fmt_opt = |x: Option<f64>| match x {
        Some(v) => fmt_usd(v),
        None => "—".into(),
    };
    let mut rows = String::new();
    for r in &d.wallets_rows {
        let net_cls = if r.net >= 0.0 { "up" } else { "down" };
        let rpn_cls = if r.rpn >= 0.0 { "up" } else { "down" };
        let flags = if r.liq {
            "<span style='color:#f97316;font-weight:700'> LIQ</span>"
        } else {
            ""
        };
        rows.push_str(&format!(
            "<tr><td class='addr'><a href='{SCAN}{}' target='_blank'>{}</a>{}</td>\
             <td>{} {}</td><td class='up'>{}</td><td class='down'>{}</td>\
             <td class='{net_cls}'>{}</td><td class='{rpn_cls}'>{}</td>\
             <td>{}</td><td>{}{}</td><td>{:.0}%</td></tr>",
            r.a,
            short_addr(&r.a),
            flags,
            r.style,
            r.tag,
            fmt_usd(r.bought),
            fmt_usd(r.sold),
            fmt_usd(r.net),
            fmt_usd(r.rpn),
            fmt_opt(r.pnl_all),
            match r.win_rate {
                Some(w) => format!("{:.0}%", w * 100.0),
                None => "—".into(),
            },
            if r.is_maker { " MKR" } else { "" },
            r.share * 100.0,
        ));
    }
    format!(
        "<div class='m-title'>{} · {} – {} <button class='m-close' id='modal-x'>CLOSE ×</button></div>\
         <div class='m-sub'>window {} – {} UTC · {} prints · {} wallets · aggressor {} vs resting {}</div>\
         <div class='m-tiles'>{}{}{}{}</div>\
         <div class='m-tiles'>{}{}{}{}</div>\
         <table class='mtab'>\
         <tr><th>wallet</th><th>style</th><th>bought</th><th>sold</th><th>net</th>\
         <th>session pnl</th><th>all-time pnl</th><th>win</th><th>share</th></tr>\
         {rows}</table>",
        d.coin,
        fmt_px(d.p_lo),
        fmt_px(d.p_hi),
        ms_hms(d.t_start),
        ms_hms(d.t_end),
        d.prints,
        d.wallets,
        fmt_usd(d.aggressor_usd),
        fmt_usd(d.resting_usd),
        tile("BUY (taker)", fmt_usd(d.buy_usd), "buy"),
        tile("SELL (taker)", fmt_usd(d.sell_usd), "sell"),
        tile(
            "DELTA",
            fmt_usd(d.delta),
            if d.delta >= 0.0 { "buy" } else { "sell" }
        ),
        tile("LIQUIDATED", fmt_usd(d.liq_usd), "sell"),
        tile(
            "OPEN",
            fmt_usd(d.actions.open),
            if d.actions.open >= d.actions.reduce { "buy" } else { "" }
        ),
        tile("INCREASE", fmt_usd(d.actions.increase), ""),
        tile("REDUCE", fmt_usd(d.actions.reduce), ""),
        tile("FLIP", fmt_usd(d.actions.flip), ""),
    )
}

// ---------------------------------------------------------------------------
// tooltip
// ---------------------------------------------------------------------------

pub fn tooltip_html(
    coin: &str,
    w: &FootWindow,
    level: Option<&shared::LevelAgg>,
    bucket: f64,
) -> String {
    let mut s = format!(
        "<div class='tt-h'>{} · {}</div>\
         <div class='row'><span>o / c</span><b>{} / {}</b></div>\
         <div class='row'><span>h / l</span><b>{} / {}</b></div>\
         <div class='row'><span>buy · sell</span><b>{} · {}</b></div>",
        coin,
        ms_hms(w.t),
        fmt_px(w.o),
        fmt_px(w.c),
        fmt_px(w.h),
        fmt_px(w.l),
        fmt_usd(w.buy),
        fmt_usd(w.sell),
    );
    if let Some(lv) = level {
        let delta = lv.delta();
        s.push_str(&format!(
            "<div class='row'><span>Δ window</span><b style='color:{}'>{}</b></div>",
            if w.delta >= 0.0 { "#1fc77e" } else { "#f0435c" },
            fmt_usd(w.delta),
        ));
        s.push_str("<div style='border-top:1px solid #1e2c3e;margin:4px 0'></div>");
        s.push_str(&format!(
            "<div class='row'><span>level {}</span><b>±{}</b></div>\
             <div class='row'><span>buy · sell</span><b>{} · {}</b></div>\
             <div class='row'><span>prints</span><b>{}</b></div>",
            fmt_px(lv.p),
            fmt_k(bucket),
            fmt_k(lv.buy),
            fmt_k(lv.sell),
            lv.n,
        ));
        if lv.liq > 0.0 {
            s.push_str(&format!(
                "<div class='row'><span style='color:#f97316'>liquidated</span><b>{}</b></div>",
                fmt_usd(lv.liq)
            ));
        }
        if lv.stop > 0.0 {
            s.push_str(&format!(
                "<div class='row'><span style='color:#ef4444'>stops</span><b>{}</b></div>",
                fmt_usd(lv.stop)
            ));
        }
        if lv.tp > 0.0 {
            s.push_str(&format!(
                "<div class='row'><span style='color:#22c55e'>take-profit</span><b>{}</b></div>",
                fmt_usd(lv.tp)
            ));
        }
        if lv.hedge > 0.0 {
            s.push_str(&format!(
                "<div class='row'><span style='color:#a78bfa'>hedge</span><b>{}</b></div>",
                fmt_usd(lv.hedge)
            ));
        }
    }
    s.push_str(&format!(
        "<div class='row'><span>prints · wallets</span><b>{} · {}</b></div>",
        w.prints, w.wallets
    ));
    s
}

// ---------------------------------------------------------------------------
// radar panel
// ---------------------------------------------------------------------------

fn lvl_state_cls(s: shared::LevelState) -> &'static str {
    match s {
        shared::LevelState::Armed => "armed",
        shared::LevelState::Engaged => "engaged",
        shared::LevelState::Swept => "swept",
    }
}

fn lvl_dist_pct(lv: &RadarLevel, px: f64) -> f64 {
    if px > 0.0 {
        (lv.px - px) / px * 100.0
    } else {
        0.0
    }
}

pub fn radar_head_html(snap: &RadarSnapshot) -> String {
    let oh = &snap.overhang;
    format!(
        "<div class='big'>{} {}</div>\
         <div class='sub'><span class='pill' style='color:#f0435c'>longs ↓ {}</span>\
         <span class='pill' style='color:#1fc77e'>shorts ↑ {}</span>within 5%</div>\
         <div class='sub'>nearest long-liq {} · nearest short-liq {}</div>",
        snap.coin,
        fmt_px(snap.px),
        fmt_usd(oh.long_usd_5),
        fmt_usd(oh.short_usd_5),
        oh.long_nearest_px
            .map(|p| format!("{} ({})", fmt_px(p), fmt_usd(oh.long_nearest_usd)))
            .unwrap_or_else(|| "—".into()),
        oh.short_nearest_px
            .map(|p| format!("{} ({})", fmt_px(p), fmt_usd(oh.short_nearest_usd)))
            .unwrap_or_else(|| "—".into()),
    )
}

pub fn radar_levels_html(snap: &RadarSnapshot) -> String {
    let px = snap.px;
    let max_est = snap
        .levels
        .iter()
        .map(|l| l.est_usd.max(l.swept_usd))
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let mut rows = String::new();
    for lv in snap.levels.iter().take(60) {
        let dist = lvl_dist_pct(lv, px);
        let side = if lv.long { "L·↓" } else { "S·↑" };
        let amt_cls = if lv.long { "long" } else { "short" };
        let bar_w = ((lv.est_usd.max(lv.swept_usd) / max_est) * 100.0).clamp(2.0, 100.0);
        rows.push_str(&format!(
            "<div class='rl-row'><span>{side} {}</span><span class='st {}'>{}</span>\
             <span class='amt {amt_cls}'>{}</span><span class='px'>{}</span>\
             <span class='dist'>{:+.2}%</span>\
             <div class='bar'><div class='fill' style='width:{bar_w:.0}%'></div></div></div>",
            fmt_k(lv.peak_usd),
            lvl_state_cls(lv.state),
            lv.state.label(),
            fmt_usd(if lv.state == shared::LevelState::Swept { lv.swept_usd } else { lv.est_usd }),
            fmt_px(lv.px),
            dist,
        ));
    }
    if rows.is_empty() {
        "<div class='warm' style='position:static;padding:26px 0'>arming magnet levels…<br>\
         <span style='font-size:9.5px'>levels populate as aggressive flow opens positions — \
         watch it fill during a volatile window</span></div>"
            .into()
    } else {
        rows
    }
}

pub fn cascade_row_html(c: &CascadeRecord) -> String {
    let dir_cls = if c.long { "long" } else { "short" };
    let dir_txt = if c.long { "LONG" } else { "SHRT" };
    let move_pct = if c.px_lo > 0.0 {
        (c.px_hi - c.px_lo) / c.px_lo * 100.0
    } else {
        0.0
    };
    format!(
        "<div class='cas-row{}'><span class='t'>{}</span><span class='c'>{}</span>\
         <span class='dir {}' title='{}s liquidated'>{}</span><span class='usd'>{}</span>\
         <span class='move'>{:.2}%</span><span class='slip'>{:.0}bp</span></div>",
        if c.live { " live" } else { "" },
        ms_hm(c.t_end),
        c.coin,
        dir_cls,
        if c.long { "longs" } else { "shorts" },
        dir_txt,
        fmt_usd(c.usd),
        move_pct,
        c.slip_bps,
    )
}

pub fn cascade_feed_html(list: &[CascadeRecord]) -> String {
    if list.is_empty() {
        return "<div class='warm' style='position:static;padding:22px 0'>no cascades yet</div>".into();
    }
    list.iter()
        .map(cascade_row_html)
        .collect::<Vec<_>>()
        .join("")
}

// ---------------------------------------------------------------------------
// opportunities panel
// ---------------------------------------------------------------------------

fn bias_html(b: Option<bool>) -> String {
    match b {
        Some(true) => "<span class='bias bull'>BULL</span>".into(),
        Some(false) => "<span class='bias bear'>BEAR</span>".into(),
        None => "<span class='bias neut'>SETUP</span>".into(),
    }
}

fn strength_color(s: f64) -> &'static str {
    if s >= 75.0 {
        "#f5c451"
    } else if s >= 55.0 {
        "#97fce4"
    } else {
        "#54677d"
    }
}

pub fn opps_html(opps: &[Opportunity]) -> String {
    if opps.is_empty() {
        return "<div class='warm' style='position:static;padding:30px 0'>no live setups — scanning…</div>"
            .into();
    }
    let mut out = String::new();
    for o in opps {
        let mut meta = format!(
            "<span>{} <b>{}</b></span><span>bias {}</span>",
            o.kind.label(),
            o.coin,
            bias_html(o.side_bias),
        );
        if let Some(tp) = o.target_px {
            meta.push_str(&format!("<span>target <b>{}</b></span>", fmt_px(tp)));
        }
        meta.push_str(&format!(
            "<span>ref <b>{}</b></span><span>ttl {}m</span>",
            fmt_px(o.ref_px),
            (o.ttl_ms / 60_000).max(1),
        ));
        out.push_str(&format!(
            "<div class='opp' style='border-left-color:{}'>\
             <div class='l1'><span class='k' style='background:{}'>{}</span>\
             <span class='c'>{}</span><span class='str'>{:.0}</span></div>\
             <div class='title'>{}</div><div class='detail'>{}</div>\
             <div class='meta'>{meta}</div>\
             <div class='strbar'><div class='fill' style='width:{:.0}%;background:{}'></div></div></div>",
            strength_color(o.strength),
            o.kind.color(),
            o.kind.label(),
            o.coin,
            o.strength,
            o.title,
            o.detail,
            o.strength.clamp(0.0, 100.0),
            strength_color(o.strength),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// tuning panel
// ---------------------------------------------------------------------------

/// one control spec; rendered as slider (range) or number input
struct Ctrl {
    key: &'static str,
    label: &'static str,
    hint: &'static str,
    min: f64,
    max: f64,
    step: f64,
    /// display transform: value -> shown text
    kind: CtrlKind,
}

enum CtrlKind {
    /// 0..1 -> percent
    Pct,
    /// raw number with unit suffix
    Raw(&'static str),
    /// ms -> seconds
    Sec,
}

impl Ctrl {
    fn to_display(&self, v: f64) -> String {
        match self.kind {
            CtrlKind::Pct => format!("{:.0}%", v * 100.0),
            CtrlKind::Raw(u) => {
                if v >= 10_000.0 {
                    format!("{}{}", fmt_k(v), u)
                } else {
                    let s = if self.step < 1.0 {
                        format!("{v:.1}")
                    } else {
                        format!("{v:.0}")
                    };
                    format!("{s}{u}")
                }
            }
            CtrlKind::Sec => format!("{:.0}s", v / 1000.0),
        }
    }
}

const GROUPS: &[(&str, &[Ctrl])] = &[
    (
        "liquidation precision (stricter LIQ)",
        &[
            Ctrl { key: "liq_conf_cap", label: "liq_conf_cap", hint: "minimum confidence for a behavioral LIQ tag — raise for fewer false positives", min: 0.05, max: 0.95, step: 0.01, kind: CtrlKind::Pct },
            Ctrl { key: "liq_min_sweep_slip_bps", label: "liq_min_sweep_slip_bps", hint: "minimum book-sweep slippage vs the 30s-ago price (bps) — liquidations sweep; voluntary stops usually don't", min: 0.0, max: 50.0, step: 0.5, kind: CtrlKind::Raw("bp") },
            Ctrl { key: "liq_min_close_frac", label: "liq_min_close_frac", hint: "fraction of the position the burst must close (0..1)", min: 0.30, max: 1.0, step: 0.01, kind: CtrlKind::Pct },
            Ctrl { key: "liq_min_loss_bps", label: "liq_min_loss_bps", hint: "minimum realized-loss depth on the close (bps of notional)", min: 0.0, max: 100.0, step: 1.0, kind: CtrlKind::Raw("bp") },
            Ctrl { key: "liq_min_usd", label: "liq_min_usd", hint: "minimum position notional eligible for a behavioral LIQ tag", min: 1000.0, max: 500_000.0, step: 1000.0, kind: CtrlKind::Raw("") },
        ],
    ),
    (
        "radar / magnet levels",
        &[
            Ctrl { key: "radar_bucket_bps", label: "radar_bucket_bps", hint: "radar grid width (bps of price)", min: 5.0, max: 100.0, step: 1.0, kind: CtrlKind::Raw("bp") },
            Ctrl { key: "radar_level_usd", label: "radar_level_usd", hint: "minimum estimated forced-flow for a level to count as a magnet", min: 5000.0, max: 1_000_000.0, step: 5000.0, kind: CtrlKind::Raw("") },
            Ctrl { key: "radar_decay_min", label: "radar_decay_min", hint: "armed-level half-life (minutes) — estimates decay as positions adjust", min: 15.0, max: 720.0, step: 15.0, kind: CtrlKind::Raw("m") },
            Ctrl { key: "radar_confirm_ms", label: "radar_confirm_ms", hint: "window a crossed level waits for forced flow before re-arming", min: 2000.0, max: 60_000.0, step: 1000.0, kind: CtrlKind::Sec },
        ],
    ),
    (
        "cascades",
        &[
            Ctrl { key: "cascade_burst_ms", label: "cascade_burst_ms", hint: "chained-burst gap that finalizes a cascade record", min: 1000.0, max: 30_000.0, step: 500.0, kind: CtrlKind::Sec },
            Ctrl { key: "cascade_max_ms", label: "cascade_max_ms", hint: "maximum cascade duration before forced finalize", min: 30_000.0, max: 600_000.0, step: 10_000.0, kind: CtrlKind::Sec },
            Ctrl { key: "cascade_min_usd", label: "cascade_min_usd", hint: "minimum forced USD for a burst to be recorded as a cascade", min: 1000.0, max: 250_000.0, step: 1000.0, kind: CtrlKind::Raw("") },
        ],
    ),
    (
        "opportunities",
        &[
            Ctrl { key: "opp_magnet_usd", label: "opp_magnet_usd", hint: "minimum armed overhang for a magnet-pull opportunity", min: 10_000.0, max: 2_000_000.0, step: 10_000.0, kind: CtrlKind::Raw("") },
            Ctrl { key: "opp_magnet_dist_pct", label: "opp_magnet_dist_pct", hint: "max distance (%) from price for a magnet to be actionable", min: 0.2, max: 5.0, step: 0.1, kind: CtrlKind::Pct },
            Ctrl { key: "opp_stop_usd", label: "opp_stop_usd", hint: "minimum stop-cluster USD for a stop-run opportunity", min: 5000.0, max: 500_000.0, step: 5000.0, kind: CtrlKind::Raw("") },
            Ctrl { key: "opp_whale_usd", label: "opp_whale_usd", hint: "minimum print size for a whale-follow opportunity", min: 25_000.0, max: 1_000_000.0, step: 25_000.0, kind: CtrlKind::Raw("") },
        ],
    ),
];

fn ctrl_value(p: &TuningParams, key: &str) -> f64 {
    match key {
        "liq_conf_cap" => p.liq_conf_cap,
        "liq_min_sweep_slip_bps" => p.liq_min_sweep_slip_bps,
        "liq_min_close_frac" => p.liq_min_close_frac,
        "liq_min_loss_bps" => p.liq_min_loss_bps,
        "liq_min_usd" => p.liq_min_usd,
        "radar_bucket_bps" => p.radar_bucket_bps,
        "radar_level_usd" => p.radar_level_usd,
        "radar_decay_min" => p.radar_decay_min,
        "radar_confirm_ms" => p.radar_confirm_ms as f64,
        "cascade_burst_ms" => p.cascade_burst_ms as f64,
        "cascade_max_ms" => p.cascade_max_ms as f64,
        "cascade_min_usd" => p.cascade_min_usd,
        "opp_magnet_usd" => p.opp_magnet_usd,
        "opp_magnet_dist_pct" => p.opp_magnet_dist_pct,
        "opp_stop_usd" => p.opp_stop_usd,
        "opp_whale_usd" => p.opp_whale_usd,
        _ => 0.0,
    }
}

pub fn tuning_html(p: &TuningParams) -> String {
    let mut out = String::new();
    for (gname, ctrls) in GROUPS {
        out.push_str(&format!("<div class='tn-group'>{gname}</div>"));
        for c in *ctrls {
            let v = ctrl_value(p, c.key);
            out.push_str(&format!(
                "<div class='tn-row'>\
                 <div class='l'><span>{}</span><b id='tn-d-{}'>{}</b></div>\
                 <input type='range' class='tn-range' data-key='{}' min='{}' max='{}' step='{}' value='{:.4}'/>\
                 <div class='hint'>{}</div></div>",
                c.label,
                c.key,
                c.to_display(v),
                c.key,
                c.min,
                c.max,
                c.step,
                v,
                c.hint,
            ));
        }
    }
    out.push_str(
        "<div class='tn-actions'>\
         <button class='tn-btn' id='tn-reset'>RESET DEFAULTS</button>\
         <button class='tn-btn' id='tn-refresh'>REFRESH</button></div>",
    );
    out
}

/// Format a value for a tuning control's live label.
pub fn ctrl_display(key: &str, v: f64) -> String {
    for (_, ctrls) in GROUPS {
        for c in *ctrls {
            if c.key == key {
                return c.to_display(v);
            }
        }
    }
    format!("{v:.2}")
}

/// Update only the displayed values of an already-rendered tuning panel.
pub fn tuning_update_values(p: &TuningParams) -> (Vec<(String, f64)>, Vec<(String, String)>) {
    let mut sliders: Vec<(String, f64)> = Vec::new();
    let mut labels: Vec<(String, String)> = Vec::new();
    for (_, ctrls) in GROUPS {
        for c in *ctrls {
            let v = ctrl_value(p, c.key);
            sliders.push((c.key.to_string(), v));
            labels.push((format!("tn-d-{}", c.key), c.to_display(v)));
        }
    }
    (sliders, labels)
}
