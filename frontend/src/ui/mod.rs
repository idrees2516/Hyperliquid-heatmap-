//! Imperative DOM panel rendering: tape rows, liquidation feed, trader
//! leaderboard, flow stats and the level drill-down modal.
//! All helpers return HTML strings; the app injects them via innerHTML.

use shared::{
    fmt_qty, fmt_usd, short_addr, FootWindow, LevelDetail, LiveLiquidation, LiveTrade, TraderRow,
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
