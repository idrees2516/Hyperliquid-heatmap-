//! Onchain Footprint — wasm frontend (Rust).
//!
//! Fully imperative: the static layout lives in index.html; this module owns
//! the state, the realtime WebSocket to the Rust backend, all DOM updates and
//! the canvas render loop. Pure Rust → wasm via wasm-bindgen / web-sys.

mod bookmap;
mod canvas;
mod ui;

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};

use shared::{
    window_start, AssetInfo, BookPayload, Candle, FootWindow, FootprintSnapshot, LevelAgg,
    LevelDetail, LiveLiquidation, LiveTrade, Opportunity, RadarSnapshot, TraderRow, TuningParams,
};
use wasm_bindgen::{closure::Closure, prelude::wasm_bindgen, JsCast, JsValue};
use web_sys::{MessageEvent, WebSocket};

use crate::ui::fmt_px;

// ---------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------

pub struct AppState {
    pub coin: String,
    pub interval: i64,
    pub assets: Vec<AssetInfo>,
    /// trade-derived footprint windows (sorted by t), updated live
    pub windows: Vec<FootWindow>,
    /// candle-only display for windows with no flow yet
    pub candle_gaps: BTreeMap<i64, Candle>,
    pub bucket: f64,
    pub tick: f64,
    pub tape: VecDeque<LiveTrade>,
    pub traders: Vec<TraderRow>,
    pub liqs: VecDeque<LiveLiquidation>,
    /// liquidation radar snapshot for the selected coin
    pub radar: Option<RadarSnapshot>,
    /// live opportunity feed (all markets)
    pub opps: Vec<Opportunity>,
    /// current engine tuning params
    pub tuning: Option<TuningParams>,
    /// active sidebar view: book | live | radar | opps | tune
    pub side_view: String,
    /// suppress tuning re-render while the user is dragging a slider
    pub tuning_interacting: bool,
    pub last_px: f64,
    pub dirty: bool,
    pub mouse: Option<(f64, f64)>,
    pub ws_open: bool,
    pub upstream: bool,
    pub poll_mode: bool,
    pub fails: u32,
    pub last_ws_try: f64,
    pub last_draw: f64,
    pub last_tt: f64,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            coin: "BTC".into(),
            interval: 60_000,
            assets: Vec::new(),
            windows: Vec::new(),
            candle_gaps: BTreeMap::new(),
            bucket: 0.0,
            tick: 0.0,
            tape: VecDeque::new(),
            traders: Vec::new(),
            liqs: VecDeque::new(),
            radar: None,
            opps: Vec::new(),
            tuning: None,
            side_view: "book".into(),
            tuning_interacting: false,
            last_px: 0.0,
            dirty: true,
            mouse: None,
            ws_open: false,
            upstream: false,
            poll_mode: false,
            fails: 0,
            last_ws_try: 0.0,
            last_draw: 0.0,
            last_tt: 0.0,
        }
    }
}

thread_local! {
    static STATE: RefCell<AppState> = RefCell::new(AppState::default());
    static WS: RefCell<Option<WebSocket>> = const { RefCell::new(None) };
    static CANVAS: RefCell<Option<web_sys::HtmlCanvasElement>> = const { RefCell::new(None) };
    static BOOK: RefCell<bookmap::BookView> = RefCell::new(bookmap::BookView::default());
    static BOOK_CANVAS: RefCell<Option<web_sys::HtmlCanvasElement>> = const { RefCell::new(None) };
    static RAF: RefCell<Option<Closure<dyn FnMut()>>> = RefCell::new(None);
    static TIMERS: RefCell<Vec<gloo_timers::callback::Interval>> = const { RefCell::new(Vec::new()) };
}

fn now() -> f64 {
    js_sys::Date::now()
}

fn doc() -> web_sys::Document {
    web_sys::window().unwrap().document().unwrap()
}

fn el<T: JsCast>(id: &str) -> T {
    doc()
        .get_element_by_id(id)
        .unwrap_or_else(|| panic!("no #{id}"))
        .dyn_into::<T>()
        .unwrap()
}

fn set_text(id: &str, s: &str) {
    el::<web_sys::Element>(id).set_text_content(Some(s));
}

fn every(ms: u32, f: impl Fn() + 'static) {
    let i = gloo_timers::callback::Interval::new(ms, f);
    TIMERS.with(|t| t.borrow_mut().push(i));
}

// ---------------------------------------------------------------------------
// fetch helper
// ---------------------------------------------------------------------------

fn fetch_json(url: String, cb: impl Fn(serde_json::Value) + 'static) {
    let w = web_sys::window().unwrap();
    let p = w.fetch_with_str(&url);
    let cb = std::rc::Rc::new(cb);
    let on_resp = Closure::<dyn FnMut(JsValue)>::new(move |resp: JsValue| {
        let r: web_sys::Response = resp.unchecked_into();
        let p2 = match r.text() {
            Ok(p) => p,
            Err(_) => return,
        };
        let cb2 = cb.clone();
        let on_text = Closure::<dyn FnMut(JsValue)>::new(move |txt: JsValue| {
            if let Some(s) = txt.as_string() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                    cb2(v);
                }
            }
        });
        let _ = p2.then(&on_text);
        on_text.forget();
    });
    let _ = p.then(&on_resp);
    on_resp.forget();
}

/// POST a JSON body and hand the parsed response to `cb`.
fn post_json(url: String, body: String, cb: impl Fn(serde_json::Value) + 'static) {
    let w = web_sys::window().unwrap();
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_body(&JsValue::from_str(&body));
    if let Ok(h) = web_sys::Headers::new() {
        let _ = h.set("Content-Type", "application/json");
        init.set_headers(&h.into());
    }
    let p = w.fetch_with_str_and_init(&url, &init);
    let cb = std::rc::Rc::new(cb);
    let on_resp = Closure::<dyn FnMut(JsValue)>::new(move |resp: JsValue| {
        let r: web_sys::Response = resp.unchecked_into();
        let p2 = match r.text() {
            Ok(p) => p,
            Err(_) => return,
        };
        let cb2 = cb.clone();
        let on_text = Closure::<dyn FnMut(JsValue)>::new(move |txt: JsValue| {
            if let Some(s) = txt.as_string() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                    cb2(v);
                }
            }
        });
        let _ = p2.then(&on_text);
        on_text.forget();
    });
    let _ = p.then(&on_resp);
    on_resp.forget();
}

// ---------------------------------------------------------------------------
// websocket
// ---------------------------------------------------------------------------

fn ws_url() -> String {
    let loc = web_sys::window().unwrap().location();
    let proto = if loc.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    let host = loc.host().unwrap_or_default();
    format!("{proto}://{host}/ws")
}

fn connect_ws() {
    STATE.with(|s| s.borrow_mut().last_ws_try = now());
    let ws = match WebSocket::new(&ws_url()) {
        Ok(w) => w,
        Err(_) => return,
    };
    WS.with(|c| *c.borrow_mut() = Some(ws.clone()));

    let onopen = Closure::<dyn FnMut()>::new(move || {
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.ws_open = true;
            st.fails = 0;
        });
        update_dots();
        send_sub_current();
        fetch_snapshot();
        let coin = STATE.with(|s| s.borrow().coin.clone());
        fetch_bookmap(&coin);
    });
    ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
    onopen.forget();

    let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        if let Some(t) = e.data().as_string() {
            handle_msg(&t);
        }
    });
    ws.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();

    let onclose = Closure::<dyn FnMut()>::new(move || {
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.ws_open = false;
            st.fails += 1;
            if st.fails >= 3 {
                st.poll_mode = true;
            }
        });
        update_dots();
    });
    ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
    onclose.forget();

    let onerr = Closure::<dyn FnMut()>::new(|| {});
    ws.set_onerror(Some(onerr.as_ref().unchecked_ref()));
    onerr.forget();
}

fn send_sub_current() {
    let coin = STATE.with(|s| s.borrow().coin.clone());
    if coin.is_empty() {
        return;
    }
    if let Some(ws) = WS.with(|c| c.borrow().clone()) {
        let _ = ws.send_with_str(&format!("{{\"type\":\"sub\",\"coin\":\"{coin}\"}}"));
    }
}

fn handle_msg(txt: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(txt) else {
        return;
    };
    match v["type"].as_str().unwrap_or("") {
        "hello" => {
            if let Some(a) = v["assets"].as_array() {
                apply_assets(serde_json::Value::Array(a.clone()));
            }
            if let Some(u) = v["upstream"].as_bool() {
                set_upstream(u);
            }
        }
        "assets" => apply_assets(v["data"].clone()),
        "status" => set_upstream(v["upstream"].as_bool().unwrap_or(false)),
        "trade" => {
            if let Ok(t) = serde_json::from_value::<LiveTrade>(v["data"].clone()) {
                on_trade(&t);
            }
        }
        "liq" => {
            if let Ok(l) = serde_json::from_value::<LiveLiquidation>(v["data"].clone()) {
                on_liq(&l);
            }
        }
        "candle" => {
            let coin = v["coin"].as_str().unwrap_or("");
            if let Ok(c) = serde_json::from_value::<Candle>(v["data"].clone()) {
                on_candle(coin, &c);
            }
        }
        "bookcol" => {
            let coin = v["coin"].as_str().unwrap_or("").to_string();
            if let Ok(col) = serde_json::from_value::<shared::BookCol>(v["data"].clone()) {
                BOOK.with(|b| b.borrow_mut().on_col(&coin, col));
            }
        }
        "booklive" => {
            let coin = v["coin"].as_str().unwrap_or("").to_string();
            if let Ok(book) = serde_json::from_value::<shared::BookCol>(v["data"].clone()) {
                BOOK.with(|b| b.borrow_mut().on_live(&coin, book));
            }
        }
        "traders" => {
            if let Ok(rows) = serde_json::from_value::<Vec<TraderRow>>(v["data"].clone()) {
                on_traders(&rows);
            }
        }
        "radar" => {
            if let Ok(snap) = serde_json::from_value::<RadarSnapshot>(v["data"].clone()) {
                on_radar(snap);
            }
        }
        "opps" => {
            if let Ok(rows) = serde_json::from_value::<Vec<Opportunity>>(v["data"].clone()) {
                on_opps(rows);
            }
        }
        "tuning" => {
            if let Ok(p) = serde_json::from_value::<TuningParams>(v["data"].clone()) {
                on_tuning(p);
            }
        }
        _ => {}
    }
}

fn set_upstream(u: bool) {
    STATE.with(|s| s.borrow_mut().upstream = u);
    update_dots();
}

fn update_dots() {
    let (open, upstream, poll) = STATE.with(|s| {
        let st = s.borrow();
        (st.ws_open, st.upstream, st.poll_mode)
    });
    let _ = el::<web_sys::Element>("dot-ws")
        .set_attribute("class", &format!("dot {}", if open { "ok" } else { "bad" }));
    let _ = el::<web_sys::Element>("dot-upstream")
        .set_attribute("class", &format!("dot {}", if upstream { "ok" } else { "bad" }));
    let _ = el::<web_sys::Element>("mode-badge")
        .set_attribute("style", &format!("display:{}", if poll { "inline-block" } else { "none" }));
}

fn apply_assets(v: serde_json::Value) {
    let Ok(assets) = serde_json::from_value::<Vec<AssetInfo>>(v) else {
        return;
    };
    let first_boot = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let first = st.coin.is_empty();
        if first {
            let pick = assets
                .iter()
                .find(|a| a.name == "BTC")
                .or_else(|| assets.first());
            if let Some(p) = pick {
                st.coin = p.name.clone();
                BOOK.with(|b| b.borrow_mut().reset(&p.name.clone()));
            }
        }
        st.assets = assets;
        first && !st.coin.is_empty()
    });
    refresh_coin_select();
    if first_boot {
        send_sub_current();
        fetch_snapshot();
        fetch_bookmap(STATE.with(|s| s.borrow().coin.clone()).as_str());
    }
    update_header_mark();
}

fn refresh_coin_select() {
    let (assets, cur) = STATE.with(|s| {
        let st = s.borrow();
        (st.assets.clone(), st.coin.clone())
    });
    let sel = el::<web_sys::HtmlSelectElement>("coin-select");
    let mut html = String::new();
    for a in assets.iter().take(60) {
        html.push_str(&format!(
            "<option value='{}'{}>{} · {}</option>",
            a.name,
            if a.name == cur { " selected" } else { "" },
            a.name,
            shared::fmt_usd(a.day_ntl_vlm),
        ));
    }
    sel.set_inner_html(&html);
    if !cur.is_empty() {
        sel.set_value(&cur);
    }
}

fn update_header_mark() {
    let (coin, last_px, assets) = STATE.with(|s| {
        let st = s.borrow();
        (st.coin.clone(), st.last_px, st.assets.clone())
    });
    let Some(a) = assets.iter().find(|a| a.name == coin) else {
        // no asset stats yet (REST degraded): still show the live price
        if last_px > 0.0 {
            set_text("mark", &fmt_px(last_px));
            set_text("chg", "");
        }
        return;
    };
    let px = if last_px > 0.0 { last_px } else { a.mark };
    let chg = a.change24h() * 100.0;
    set_text("mark", &fmt_px(px));
    let _ = el::<web_sys::Element>("chg")
        .set_attribute("class", if chg >= 0.0 { "up" } else { "down" });
    set_text("chg", &format!("{:+.2}%", chg));
}

// ---------------------------------------------------------------------------
// live events
// ---------------------------------------------------------------------------

fn on_trade(t: &LiveTrade) {
    let is_sel = STATE.with(|s| s.borrow().coin == t.coin);
    if !is_sel {
        return;
    }
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.last_px = t.px;
        st.tape.push_front(t.clone());
        while st.tape.len() > 80 {
            st.tape.pop_back();
        }
        apply_trade(&mut st, t);
        st.dirty = true;
    });
    let scroll = el::<web_sys::Element>("tape-scroll");
    if let Ok(_) = scroll.insert_adjacent_html("afterbegin", &ui::tape_row_html(t)) {
        while scroll.child_element_count() > 60 {
            let node: &web_sys::Node = scroll.as_ref();
            if let Some(last) = scroll.last_element_child() {
                let _ = node.remove_child(last.as_ref());
            } else {
                break;
            }
        }
    }
}

/// Fold a live trade into the current footprint state (window + level cells).
fn apply_trade(st: &mut AppState, t: &LiveTrade) {
    if st.bucket <= 0.0 {
        return;
    }
    let w = window_start(t.t, st.interval);
    let center = ((t.px / st.bucket).floor() + 0.5) * st.bucket;
    let idx = st.windows.iter().position(|x| x.t == w);
    match idx {
        Some(i) => {
            let win = &mut st.windows[i];
            win.h = win.h.max(t.px);
            win.l = if win.l <= 0.0 { t.px } else { win.l.min(t.px) };
            win.c = t.px;
            if win.o <= 0.0 {
                win.o = t.px;
            }
            win.prints += 1;
            if t.side.is_buy() {
                win.buy += t.usd;
            } else {
                win.sell += t.usd;
            }
            win.delta = win.buy - win.sell;
            let buy = t.side.is_buy();
            if let Some(lv) = win
                .levels
                .iter_mut()
                .find(|l| (l.p - center).abs() < st.bucket * 0.25)
            {
                lv.n += 1;
                if buy {
                    lv.buy += t.usd;
                } else {
                    lv.sell += t.usd;
                }
                match t.intent {
                    shared::Intent::Liquidation | shared::Intent::Deleveraging => lv.liq += t.usd,
                    shared::Intent::StopLoss => lv.stop += t.usd,
                    shared::Intent::TakeProfit => lv.tp += t.usd,
                    shared::Intent::Hedge => lv.hedge += t.usd,
                    _ => {}
                }
            } else {
                let mut lv = LevelAgg {
                    p: center,
                    ..Default::default()
                };
                if buy {
                    lv.buy = t.usd;
                } else {
                    lv.sell = t.usd;
                }
                lv.n = 1;
                match t.intent {
                    shared::Intent::Liquidation | shared::Intent::Deleveraging => lv.liq = t.usd,
                    shared::Intent::StopLoss => lv.stop = t.usd,
                    shared::Intent::TakeProfit => lv.tp = t.usd,
                    shared::Intent::Hedge => lv.hedge = t.usd,
                    _ => {}
                }
                win.levels.push(lv);
                win.levels
                    .sort_by(|a, b| a.p.partial_cmp(&b.p).unwrap_or(std::cmp::Ordering::Equal));
            }
        }
        None => {
            let (buy, sell) = if t.side.is_buy() {
                (t.usd, 0.0)
            } else {
                (0.0, t.usd)
            };
            let lv = LevelAgg {
                p: center,
                buy,
                sell,
                n: 1,
                liq: if matches!(
                    t.intent,
                    shared::Intent::Liquidation | shared::Intent::Deleveraging
                ) {
                    t.usd
                } else {
                    0.0
                },
                stop: if matches!(t.intent, shared::Intent::StopLoss) {
                    t.usd
                } else {
                    0.0
                },
                tp: if matches!(t.intent, shared::Intent::TakeProfit) {
                    t.usd
                } else {
                    0.0
                },
                hedge: if matches!(t.intent, shared::Intent::Hedge) {
                    t.usd
                } else {
                    0.0
                },
            };
            st.windows.push(FootWindow {
                t: w,
                o: t.px,
                h: t.px,
                l: t.px,
                c: t.px,
                buy,
                sell,
                delta: buy - sell,
                poc: center,
                wallets: 1,
                prints: 1,
                levels: vec![lv],
                tb: vec![],
                ts: vec![],
            });
            st.candle_gaps.remove(&w);
            while st.windows.len() > 160 {
                st.windows.remove(0);
            }
        }
    }
}

fn on_liq(l: &LiveLiquidation) {
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.liqs.push_front(l.clone());
        while st.liqs.len() > 40 {
            st.liqs.pop_back();
        }
    });
    let feed = el::<web_sys::Element>("liq-feed");
    if let Ok(_) = feed.insert_adjacent_html("afterbegin", &ui::liq_row_html(l)) {
        while feed.child_element_count() > 40 {
            let node: &web_sys::Node = feed.as_ref();
            if let Some(last) = feed.last_element_child() {
                let _ = node.remove_child(last.as_ref());
            } else {
                break;
            }
        }
    }
    // bookmap sidebar feed
    let bfeed = el::<web_sys::Element>("book-liq-feed");
    let _ = bfeed.insert_adjacent_html("afterbegin", &ui::liq_row_html(l));
    while bfeed.child_element_count() > 40 {
        let node: &web_sys::Node = bfeed.as_ref();
        if let Some(last) = bfeed.last_element_child() {
            let _ = node.remove_child(last.as_ref());
        } else {
            break;
        }
    }
    set_text("liq-count", &format!("×{}", STATE.with(|s| s.borrow().liqs.len())));
}

fn on_candle(coin: &str, c: &Candle) {
    let is_sel = STATE.with(|s| s.borrow().coin == coin);
    if !is_sel {
        return;
    }
    STATE.with(|s| {
        let mut st = s.borrow_mut();
        let w = window_start(c.t, st.interval);
        match st.windows.iter_mut().find(|x| x.t == w) {
            Some(win) => {
                if win.prints == 0 {
                    win.o = c.o;
                    win.h = c.h;
                    win.l = c.l;
                    win.c = c.c;
                } else {
                    win.h = win.h.max(c.h);
                    win.l = win.l.min(c.l);
                    win.c = c.c;
                }
            }
            None => {
                st.candle_gaps
                    .entry(w)
                    .and_modify(|old| {
                        old.o = c.o;
                        old.c = c.c;
                        old.h = c.h;
                        old.l = c.l;
                    })
                    .or_insert_with(|| c.clone());
            }
        }
        st.dirty = true;
    });
}

fn on_traders(rows: &[TraderRow]) {
    STATE.with(|s| s.borrow_mut().traders = rows.to_vec());
    el::<web_sys::Element>("traders-scroll").set_inner_html(&ui::traders_html(rows));
}

// ---------------------------------------------------------------------------
// radar / opps / tuning (live panels)
// ---------------------------------------------------------------------------

fn on_radar(snap: RadarSnapshot) {
    let (view, is_sel) = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let sel = st.coin == snap.coin;
        if sel {
            st.radar = Some(snap.clone());
        }
        (st.side_view.clone(), sel)
    });
    if !is_sel {
        return;
    }
    let snap_ref = STATE.with(|s| s.borrow().radar.clone());
    let Some(snap) = snap_ref else { return };
    el::<web_sys::Element>("radar-head").set_inner_html(&ui::radar_head_html(&snap));
    el::<web_sys::Element>("radar-levels").set_inner_html(&ui::radar_levels_html(&snap));
    el::<web_sys::Element>("cascade-feed").set_inner_html(&ui::cascade_feed_html(&snap.last_cascade));
    set_text(
        "cascade-count",
        &format!("×{}", snap.last_cascade.len().min(99)),
    );
    // radar tab badge: armed levels near price
    let near = snap
        .levels
        .iter()
        .filter(|l| {
            l.state == shared::LevelState::Armed
                && snap.px > 0.0
                && ((l.px - snap.px) / snap.px).abs() <= 0.05
        })
        .count();
    let badge = if near > 0 { near.to_string() } else { String::new() };
    set_text("radar-badge", &badge);
    let _ = view;
}

fn on_opps(rows: Vec<Opportunity>) {
    // drop expired rows (TTL)
    let now = now() as i64;
    let live: Vec<Opportunity> = rows
        .into_iter()
        .filter(|o| now - o.t <= o.ttl_ms)
        .collect();
    let n = live.len();
    STATE.with(|s| s.borrow_mut().opps = live.clone());
    el::<web_sys::Element>("opps-scroll").set_inner_html(&ui::opps_html(&live));
    let badge = if n > 0 { n.to_string() } else { String::new() };
    set_text("opps-badge", &badge);
    set_text(
        "opps-note",
        &format!("{} live setups · all markets", n),
    );
}

fn on_tuning(p: TuningParams) {
    let interacting = STATE.with(|s| {
        let mut st = s.borrow_mut();
        let was = st.tuning_interacting;
        st.tuning = Some(p.clone());
        !was
    });
    if !interacting {
        return; // user is mid-drag; values re-render on release
    }
    render_tuning(&p);
}

fn render_tuning(p: &TuningParams) {
    el::<web_sys::Element>("tune-scroll").set_inner_html(&ui::tuning_html(p));
    wire_tuning_controls();
}

fn on_tuning_saved(v: serde_json::Value) {
    if let Ok(p) = serde_json::from_value::<TuningParams>(v) {
        render_tuning(&p);
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.tuning = Some(p);
        });
    }
}

fn fetch_radar() {
    let coin = STATE.with(|s| s.borrow().coin.clone());
    if coin.is_empty() {
        return;
    }
    fetch_json(format!("/api/radar?coin={coin}"), move |v| {
        if let Ok(snap) = serde_json::from_value::<RadarSnapshot>(v) {
            on_radar(snap);
        }
    });
}

fn fetch_opps() {
    fetch_json("/api/opps".into(), move |v| {
        if let Ok(rows) = serde_json::from_value::<Vec<Opportunity>>(v["opps"].clone()) {
            on_opps(rows);
        }
    });
}

fn fetch_tuning() {
    fetch_json("/api/tuning".into(), move |v| {
        if let Ok(p) = serde_json::from_value::<TuningParams>(v) {
            on_tuning(p);
        }
    });
}

/// POST one tuned field to the engine (live update).
fn post_tuning_field(key: &str, value: f64) {
    let body = format!("{{\"{key}\":{value}}}");
    post_json("/api/tuning".into(), body, on_tuning_saved);
}

fn post_tuning_reset() {
    post_json("/api/tuning/reset".into(), "{}".into(), on_tuning_saved);
}

// ---------------------------------------------------------------------------
// snapshot
// ---------------------------------------------------------------------------

fn update_tuning_label(key: &str, v: f64) {
    set_text(&format!("tn-d-{key}"), &ui::ctrl_display(key, v));
}

/// Attach change listeners to the rendered tuning controls.
fn wire_tuning_controls() {
    let coll = doc().get_elements_by_class_name("tn-range");
    for i in 0..coll.length() {
        let Some(node) = coll.item(i) else { continue };
        let Ok(inp) = node.dyn_into::<web_sys::HtmlInputElement>() else { continue };
        let key = inp.get_attribute("data-key").unwrap_or_default();
        // live label update while dragging
        let k2 = key.clone();
        let on_input = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
            let Ok(t) = e
                .target()
                .unwrap()
                .dyn_into::<web_sys::HtmlInputElement>()
            else {
                return;
            };
            let v: f64 = t.value().parse().unwrap_or(0.0);
            update_tuning_label(&k2, v);
            STATE.with(|s| s.borrow_mut().tuning_interacting = true);
        });
        inp.add_event_listener_with_callback("input", on_input.as_ref().unchecked_ref())
            .unwrap();
        on_input.forget();
        // release: POST the new value to the engine
        let k3 = key.clone();
        let on_change = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
            let Ok(t) = e
                .target()
                .unwrap()
                .dyn_into::<web_sys::HtmlInputElement>()
            else {
                return;
            };
            let v: f64 = t.value().parse().unwrap_or(0.0);
            post_tuning_field(&k3, v);
            STATE.with(|s| s.borrow_mut().tuning_interacting = false);
        });
        inp.add_event_listener_with_callback("change", on_change.as_ref().unchecked_ref())
            .unwrap();
        on_change.forget();
    }
    if let Some(b) = doc().get_element_by_id("tn-reset") {
        let onc = Closure::<dyn FnMut(web_sys::Event)>::new(|_| post_tuning_reset());
        b.add_event_listener_with_callback("click", onc.as_ref().unchecked_ref())
            .unwrap();
        onc.forget();
    }
    if let Some(b) = doc().get_element_by_id("tn-refresh") {
        let onc = Closure::<dyn FnMut(web_sys::Event)>::new(|_| fetch_tuning());
        b.add_event_listener_with_callback("click", onc.as_ref().unchecked_ref())
            .unwrap();
        onc.forget();
    }
}

/// Switch the sidebar between BOOK / LIVE / RADAR / OPPS / TUNE.
fn switch_side_view(view: &str) {
    let tabs = doc().get_elements_by_class_name("side-tab");
    for i in 0..tabs.length() {
        if let Some(b) = tabs.item(i) {
            let is = b
                .get_attribute("data-view")
                .map(|v| v == view)
                .unwrap_or(false);
            let _ = b.set_attribute(
                "class",
                if is { "side-tab active" } else { "side-tab" },
            );
        }
    }
    let views = doc().get_elements_by_class_name("side-view");
    for i in 0..views.length() {
        if let Some(v) = views.item(i) {
            let is = v.id() == format!("view-{view}");
            let _ = v.set_attribute(
                "class",
                if is { "side-view active" } else { "side-view" },
            );
        }
    }
    // main area: bookmap terminal vs footprint chart
    let book_active = view == "book";
    let _ = el::<web_sys::Element>("live-pane").set_attribute(
        "style",
        if book_active { "display:none" } else { "display:flex" },
    );
    let _ = el::<web_sys::Element>("book-pane").set_attribute(
        "style",
        if book_active { "display:flex" } else { "display:none" },
    );
    STATE.with(|s| s.borrow_mut().side_view = view.to_string());
    if view == "radar" {
        fetch_radar();
    }
    if view == "book" {
        fetch_bookmap_if_stale();
    }
    let need_tuning = STATE.with(|s| s.borrow().tuning.is_none());
    if view == "tune" && need_tuning {
        fetch_tuning();
    }
}

/// Fetch the bookmap history snapshot when missing / stale / coin changed.
fn fetch_bookmap_if_stale() {
    let coin = STATE.with(|s| s.borrow().coin.clone());
    if coin.is_empty() {
        return;
    }
    let stale = BOOK.with(|b| b.borrow().stale(&coin));
    if stale {
        fetch_bookmap(&coin);
    }
}

fn fetch_bookmap(coin: &str) {
    let coin = coin.to_string();
    fetch_json(
        format!("/api/bookmap?coin={coin}&max_cols=1600&trades=700"),
        move |v| {
            if let Ok(p) = serde_json::from_value::<BookPayload>(v) {
                BOOK.with(|b| b.borrow_mut().on_seed(p));
            }
        },
    );
}

fn fetch_snapshot() {
    let (coin, interval) = STATE.with(|s| {
        let st = s.borrow();
        (st.coin.clone(), st.interval)
    });
    if coin.is_empty() {
        return;
    }
    fetch_json(
        format!("/api/footprint?coin={coin}&interval={interval}&limit=120&bucket_mult=1"),
        move |v| {
            let Ok(snap) = serde_json::from_value::<FootprintSnapshot>(v) else {
                return;
            };
            STATE.with(|s| {
                let mut st = s.borrow_mut();
                st.windows = snap.windows.clone();
                st.bucket = snap.bucket;
                st.tick = snap.tick;
                st.last_px = st
                    .windows
                    .last()
                    .map(|w| w.c)
                    .filter(|c| *c > 0.0)
                    .unwrap_or(st.last_px);
                st.candle_gaps.clear();
                let iv = st.interval;
                for c in &snap.candles {
                    st.candle_gaps.insert(window_start(c.t, iv), c.clone());
                }
                let wts: Vec<i64> = st.windows.iter().map(|w| w.t).collect();
                for t in wts {
                    st.candle_gaps.remove(&t);
                }
                st.dirty = true;
            });
        },
    );
}

fn fetch_assets() {
    fetch_json("/api/assets".into(), move |v| {
        if let Some(a) = v["assets"].as_array() {
            apply_assets(serde_json::Value::Array(a.clone()));
        }
        if let Some(u) = v["upstream"].as_bool() {
            set_upstream(u);
        }
    });
}

fn fetch_traders_rest() {
    let coin = STATE.with(|s| s.borrow().coin.clone());
    if coin.is_empty() {
        return;
    }
    fetch_json(
        format!("/api/traders?coin={coin}&minutes=60"),
        move |v| {
            if let Ok(rows) = serde_json::from_value::<Vec<TraderRow>>(v["data"].clone()) {
                on_traders(&rows);
            }
        },
    );
}

// ---------------------------------------------------------------------------
// flow / header ticks
// ---------------------------------------------------------------------------

fn flow_tick() {
    let sums = STATE.with(|s| ui::flow_sums(&s.borrow().windows));
    let delta = sums.buy - sums.sell;
    let total = sums.buy + sums.sell;
    let d = el::<web_sys::Element>("flow-delta");
    d.set_text_content(Some(&shared::fmt_usd(delta)));
    let _ = d.set_attribute("class", if delta >= 0.0 { "up" } else { "down" });
    let bb = if total > 0.0 { sums.buy / total * 100.0 } else { 50.0 };
    el::<web_sys::Element>("flow-bar-b")
        .set_attribute("style", &format!("width:{bb:.1}%"))
        .ok();
    el::<web_sys::Element>("flow-bar-s")
        .set_attribute("style", &format!("width:{:.1}%", 100.0 - bb))
        .ok();
    set_text(
        "flow-buy",
        &format!("BUY {} ({:.0}%)", shared::fmt_usd(sums.buy), bb),
    );
    set_text(
        "flow-sell",
        &format!("SELL {} ({:.0}%)", shared::fmt_usd(sums.sell), 100.0 - bb),
    );
    set_text(
        "f-liq",
        &shared::fmt_usd(sums.liq),
    );
    set_text("f-stop", &shared::fmt_usd(sums.stop));
    set_text("f-tp", &shared::fmt_usd(sums.tp));
    set_text("f-hedge", &shared::fmt_usd(sums.hedge));
    set_text(
        "f-prints",
        &format!("{} prints · {} wallets", sums.prints, sums.wallets),
    );
    set_text(
        "flow-note",
        &format!("net taker delta · ~{}m span", sums.span_min),
    );
    update_header_mark();

    // bookmap sidebar: ±1% book pressure from the live book
    BOOK.with(|b| {
        let bv = b.borrow();
        if let Some(book) = bv.cur.as_ref() {
            let mid = book.mid();
            if mid > 0.0 {
                let lo = mid * 0.99;
                let hi = mid * 1.01;
                let bid: f64 = book
                    .bids
                    .iter()
                    .take_while(|l| l.px >= lo)
                    .map(|l| l.px * l.sz)
                    .sum();
                let ask: f64 = book
                    .asks
                    .iter()
                    .take_while(|l| l.px <= hi)
                    .map(|l| l.px * l.sz)
                    .sum();
                let tot = bid + ask;
                if tot > 0.0 {
                    let pb = bid / tot * 100.0;
                    set_text("bp-bid", &shared::fmt_usd(bid));
                    set_text("bp-ask", &shared::fmt_usd(ask));
                    let _ = el::<web_sys::Element>("bp-bar-b")
                        .set_attribute("style", &format!("width:{pb:.1}%"));
                    let _ = el::<web_sys::Element>("bp-bar-s")
                        .set_attribute("style", &format!("width:{:.1}%", 100.0 - pb));
                    set_text("bp-imb", &format!("{:.0} / {:.0}", pb, 100.0 - pb));
                    set_text("bp-spread", &format!("spread {:.1}bp", book.spread_bps()));
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// canvas events
// ---------------------------------------------------------------------------

fn canvas_pos(e: &web_sys::MouseEvent) -> Option<(f64, f64)> {
    let canvas = el::<web_sys::HtmlCanvasElement>("chart");
    let r = canvas.get_bounding_client_rect();
    Some((e.client_x() as f64 - r.left(), e.client_y() as f64 - r.top()))
}

fn book_canvas_pos(e: &web_sys::MouseEvent) -> Option<(f64, f64)> {
    let canvas = el::<web_sys::HtmlCanvasElement>("book-canvas");
    let r = canvas.get_bounding_client_rect();
    Some((e.client_x() as f64 - r.left(), e.client_y() as f64 - r.top()))
}

fn update_tooltip(x: f64, y: f64) {
    let now_t = now();
    let allow = STATE.with(|s| {
        let mut st = s.borrow_mut();
        st.mouse = Some((x, y));
        let ok = now_t - st.last_tt > 60.0;
        if ok {
            st.last_tt = now_t;
        }
        ok
    });
    if !allow {
        return;
    }
    let Some(hit) = canvas::hit_test(x, y) else {
        let tt = el::<web_sys::HtmlElement>("tooltip");
        tt.style().set_property("display", "none").ok();
        return;
    };
    let html = STATE.with(|s| {
        let st = s.borrow();
        let win = st.windows.iter().find(|w| w.t == hit.t);
        let level = win.and_then(|w| {
            hit.level.and_then(|p| {
                w.levels
                    .iter()
                    .find(|l| (l.p - p).abs() < st.bucket.max(1e-9) * 0.25)
            })
        });
        let gap = st.candle_gaps.get(&hit.t);
        match (win, level) {
            (Some(w), l) => Some(ui::tooltip_html(&st.coin, w, l, st.bucket)),
            (None, Some(_)) => None,
            (None, None) => gap.map(|c| {
                format!(
                    "<div class='tt-h'>{} · {}</div>\
                     <div class='row'><span>o / c</span><b>{} / {}</b></div>\
                     <div class='row'><span>h / l</span><b>{} / {}</b></div>\
                     <div class='row'><span>no taker flow</span><b>—</b></div>",
                    st.coin,
                    ui::ms_hm(hit.t),
                    fmt_px(c.o),
                    fmt_px(c.c),
                    fmt_px(c.h),
                    fmt_px(c.l),
                )
            }),
        }
    });
    let tt = el::<web_sys::HtmlElement>("tooltip");
    match html {
        Some(h) => {
            tt.set_inner_html(&h);
            tt.style().set_property("display", "block").ok();
            let wrap = el::<web_sys::Element>("chart-wrap");
            let (ww, _wh) = (wrap.client_width() as f64, wrap.client_height() as f64);
            let (mut tx, ty) = (x + 16.0, y + 14.0);
            if tx + 240.0 > ww {
                tx = (x - 250.0).max(4.0);
            }
            tt.style()
                .set_property("left", &format!("{tx:.0}px"))
                .ok();
            tt.style()
                .set_property("top", &format!("{ty:.0}px"))
                .ok();
        }
        None => {
            tt.style().set_property("display", "none").ok();
        }
    }
}

fn on_canvas_click(x: f64, y: f64) {
    let Some(hit) = canvas::hit_test(x, y) else { return };
    let (coin, interval, bucket) = STATE.with(|s| {
        let st = s.borrow();
        (st.coin.clone(), st.interval, st.bucket)
    });
    if coin.is_empty() || bucket <= 0.0 {
        return;
    }
    // clicked a specific level cell -> that level; otherwise the window's POC
    let level_p = match hit.level {
        Some(p) => p,
        None => {
            let poc = STATE.with(|s| {
                let st = s.borrow();
                st.windows
                    .iter()
                    .find(|w| w.t == hit.t)
                    .map(|w| {
                        if w.poc > 0.0 {
                            w.poc
                        } else {
                            w.levels
                                .iter()
                                .max_by(|a, b| {
                                    a.total()
                                        .partial_cmp(&b.total())
                                        .unwrap_or(std::cmp::Ordering::Equal)
                                })
                                .map(|l| l.p)
                                .unwrap_or(0.0)
                        }
                    })
                    .unwrap_or(0.0)
            });
            if poc <= 0.0 {
                return;
            }
            poc
        }
    };
    let p_lo = level_p - bucket / 2.0;
    fetch_json(
        format!(
            "/api/level?coin={coin}&t={}&interval={interval}&p_lo={p_lo:.10}&bucket_mult=1",
            hit.t
        ),
        move |v| {
            let Ok(d) = serde_json::from_value::<LevelDetail>(v) else {
                return;
            };
            el::<web_sys::Element>("modal-body").set_inner_html(&ui::level_modal_html(&d));
            let _ = el::<web_sys::Element>("modal").set_attribute("class", "open");
        },
    );
}

fn close_modal() {
    let _ = el::<web_sys::Element>("modal").set_attribute("class", "");
}

// ---------------------------------------------------------------------------
// boot
// ---------------------------------------------------------------------------

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();

    // canvas + events
    let canvas = el::<web_sys::HtmlCanvasElement>("chart");
    {
        let c2 = canvas.clone();
        let mv = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |e| {
            if let Some((x, y)) = canvas_pos(&e) {
                update_tooltip(x, y);
                STATE.with(|s| s.borrow_mut().dirty = true);
            }
        });
        c2.set_onmousemove(Some(mv.as_ref().unchecked_ref()));
        mv.forget();
    }
    {
        let c2 = canvas.clone();
        let lv = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            STATE.with(|s| s.borrow_mut().mouse = None);
            el::<web_sys::HtmlElement>("tooltip")
                .style()
                .set_property("display", "none")
                .ok();
        });
        c2.set_onmouseleave(Some(lv.as_ref().unchecked_ref()));
        lv.forget();
    }
    {
        let c2 = canvas.clone();
        let cl = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |e| {
            if let Some((x, y)) = canvas_pos(&e) {
                on_canvas_click(x, y);
            }
        });
        c2.set_onclick(Some(cl.as_ref().unchecked_ref()));
        cl.forget();
    }
    CANVAS.with(|c| *c.borrow_mut() = Some(canvas));

    // coin select
    {
        let sel = el::<web_sys::HtmlSelectElement>("coin-select");
        let onch = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
            let target = e.target().unwrap();
            let sel: web_sys::HtmlSelectElement = target.dyn_into().unwrap();
            let coin = sel.value();
            if coin.is_empty() {
                return;
            }
            STATE.with(|s| {
                let mut st = s.borrow_mut();
                if st.coin == coin {
                    return;
                }
                st.coin = coin.clone();
                st.windows.clear();
                st.candle_gaps.clear();
                st.tape.clear();
                st.traders.clear();
                st.radar = None;
                st.last_px = 0.0;
                st.dirty = true;
            });
            el::<web_sys::Element>("tape-scroll").set_inner_html("");
            el::<web_sys::Element>("traders-scroll").set_inner_html("");
            el::<web_sys::Element>("radar-levels").set_inner_html("");
            el::<web_sys::Element>("cascade-feed").set_inner_html("");
            el::<web_sys::Element>("book-liq-feed").set_inner_html("");
            BOOK.with(|b| b.borrow_mut().reset(&coin));
            send_sub_current();
            fetch_snapshot();
            fetch_radar();
            fetch_bookmap(&coin);
            update_header_mark();
        });
        sel.set_onchange(Some(onch.as_ref().unchecked_ref()));
        onch.forget();
    }

    // interval buttons
    {
        let coll = doc().get_elements_by_class_name("iv-btn");
        for i in 0..coll.length() {
            if let Some(b) = coll.item(i) {
                let iv = b
                    .get_attribute("data-iv")
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(60_000);
                let oncl = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
                    let target = e.target().unwrap();
                    let b: web_sys::HtmlElement = target.dyn_into().unwrap();
                    let my_iv = b.get_attribute("data-iv");
                    // toggle active class on all buttons
                    let coll2 = doc().get_elements_by_class_name("iv-btn");
                    for j in 0..coll2.length() {
                        if let Some(x) = coll2.item(j) {
                            let is_me = x.get_attribute("data-iv") == my_iv;
                            let cur = x.get_attribute("class").unwrap_or_default();
                            let base = cur.replace(" active", "");
                            x.set_attribute(
                                "class",
                                &format!("{base}{}", if is_me { " active" } else { "" }),
                            )
                            .ok();
                        }
                    }
                    STATE.with(|s| {
                        let mut st = s.borrow_mut();
                        if st.interval != iv {
                            st.interval = iv;
                            st.dirty = true;
                        }
                    });
                    fetch_snapshot();
                });
                b.add_event_listener_with_callback("click", oncl.as_ref().unchecked_ref())
                    .unwrap();
                oncl.forget();
            }
        }
    }

    // ---- bookmap terminal: canvas + toolbar --------------------------------
    {
        let bcanvas = el::<web_sys::HtmlCanvasElement>("book-canvas");
        {
            let c2 = bcanvas.clone();
            let mv = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |e| {
                if let Some((x, y)) = book_canvas_pos(&e) {
                    BOOK.with(|b| b.borrow_mut().hover = Some((x, y)));
                }
            });
            c2.set_onmousemove(Some(mv.as_ref().unchecked_ref()));
            mv.forget();
        }
        {
            let c2 = bcanvas.clone();
            let lv = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                BOOK.with(|b| b.borrow_mut().hover = None);
            });
            c2.set_onmouseleave(Some(lv.as_ref().unchecked_ref()));
            lv.forget();
        }
        {
            let c2 = bcanvas.clone();
            let wh = Closure::<dyn FnMut(web_sys::WheelEvent)>::new(move |e: web_sys::WheelEvent| {
                e.prevent_default();
                BOOK.with(|b| bookmap::wheel(&mut b.borrow_mut(), e.delta_y()));
            });
            c2.set_onwheel(Some(wh.as_ref().unchecked_ref()));
            wh.forget();
        }
        {
            let c2 = bcanvas.clone();
            let db = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                BOOK.with(|b| {
                    let mut bv = b.borrow_mut();
                    bv.set_intensity(0.0);
                    bv.invalidate();
                });
                let slider = el::<web_sys::HtmlInputElement>("bt-intensity");
                slider.set_value("0");
            });
            c2.set_ondblclick(Some(db.as_ref().unchecked_ref()));
            db.forget();
        }
        BOOK_CANVAS.with(|c| *c.borrow_mut() = Some(bcanvas));

        // range presets
        let coll = doc().get_elements_by_class_name("bt-btn");
        for i in 0..coll.length() {
            if let Some(b) = coll.item(i) {
                let Some(cols) = b.get_attribute("data-cols").and_then(|v| v.parse::<usize>().ok()) else {
                    continue;
                };
                let oncl = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
                    let target = e.target().unwrap();
                    let btn: web_sys::HtmlElement = target.dyn_into().unwrap();
                    let active = btn.get_attribute("class").unwrap_or_default().contains("active");
                    if !active {
                        // range buttons are exclusive
                        let all = doc().get_elements_by_class_name("bt-btn");
                        for j in 0..all.length() {
                            if let Some(x) = all.item(j) {
                                if x.get_attribute("data-cols").is_some() {
                                    let _ = x.set_attribute("class", "bt-btn");
                                }
                            }
                        }
                        let _ = btn.set_attribute("class", "bt-btn active");
                        BOOK.with(|b| {
                            let mut bv = b.borrow_mut();
                            bv.vis_cols = cols;
                            bv.invalidate();
                        });
                    }
                });
                b.add_event_listener_with_callback("click", oncl.as_ref().unchecked_ref())
                    .unwrap();
                oncl.forget();
            }
        }

        // heat intensity slider
        {
            let slider = el::<web_sys::HtmlInputElement>("bt-intensity");
            let onin = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
                let target = e.target().unwrap();
                let s: web_sys::HtmlInputElement = target.dyn_into().unwrap();
                let v = s.value().parse::<f64>().unwrap_or(0.0) / 100.0;
                BOOK.with(|b| b.borrow_mut().set_intensity(v));
            });
            slider.set_oninput(Some(onin.as_ref().unchecked_ref()));
            onin.forget();
        }

        // panel toggles
        let coll = doc().get_elements_by_class_name("tgl");
        for i in 0..coll.length() {
            if let Some(b) = coll.item(i) {
                let Some(key) = b.get_attribute("data-t") else { continue };
                let oncl = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
                    let target = e.target().unwrap();
                    let btn: web_sys::HtmlElement = target.dyn_into().unwrap();
                    let cur = btn.get_attribute("class").unwrap_or_default();
                    let on = cur.contains("active");
                    let _ = btn.set_attribute(
                        "class",
                        &format!("bt-btn tgl{}", if on { "" } else { " active" }),
                    );
                    BOOK.with(|b| {
                        let mut bv = b.borrow_mut();
                        match key.as_str() {
                            "trades" => bv.show_trades = !on,
                            "liqs" => bv.show_liqs = !on,
                            "profile" => {
                                bv.show_profile = !on;
                                bv.invalidate();
                            }
                            "dom" => {
                                bv.show_dom = !on;
                                bv.invalidate();
                            }
                            "radar" => {
                                bv.show_radar = !on;
                                bv.invalidate();
                            }
                            "vwap" => bv.show_vwap = !on,
                            "delta" => {
                                bv.show_delta = !on;
                                bv.invalidate();
                            }
                            _ => {}
                        }
                    });
                });
                b.add_event_listener_with_callback("click", oncl.as_ref().unchecked_ref())
                    .unwrap();
                oncl.forget();
            }
        }

        // Recenter: back to the live edge (default zoom, auto price, neutral heat)
        {
            let btn = el::<web_sys::HtmlElement>("bt-recenter");
            let onc = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                BOOK.with(|b| b.borrow_mut().recenter());
                let slider = el::<web_sys::HtmlInputElement>("bt-intensity");
                slider.set_value("0");
                // restore the ~12m range preset as active
                let all = doc().get_elements_by_class_name("bt-btn");
                for j in 0..all.length() {
                    if let Some(x) = all.item(j) {
                        if let Some(c) = x.get_attribute("data-cols") {
                            let _ = x.set_attribute(
                                "class",
                                if c == "1800" { "bt-btn active" } else { "bt-btn" },
                            );
                        }
                    }
                }
            });
            btn.add_event_listener_with_callback("click", onc.as_ref().unchecked_ref())
                .unwrap();
            onc.forget();
        }
    }

    // modal close (delegation) + ESC
    {
        let onc = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
            if let Some(t) = e.target() {
                if let Ok(x) = t.dyn_into::<web_sys::Element>() {
                    let cls = x.get_attribute("class").unwrap_or_default();
                    if x.id() == "modal" || cls.contains("m-close") {
                        close_modal();
                    }
                }
            }
        });
        let md = el::<web_sys::Element>("modal");
        md.add_event_listener_with_callback("click", onc.as_ref().unchecked_ref())
            .unwrap();
        onc.forget();
    }
    {
        let kd = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            if e.key() == "Escape" {
                close_modal();
            }
        });
        doc().set_onkeydown(Some(kd.as_ref().unchecked_ref()));
        kd.forget();
    }

    // sidebar tabs: LIVE / RADAR / OPPS / TUNE
    {
        let coll = doc().get_elements_by_class_name("side-tab");
        for i in 0..coll.length() {
            if let Some(b) = coll.item(i) {
                let view = b.get_attribute("data-view").unwrap_or_default();
                let onc = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                    switch_side_view(&view);
                });
                b.add_event_listener_with_callback("click", onc.as_ref().unchecked_ref())
                    .unwrap();
                onc.forget();
            }
        }
    }

    // periodic ticks
    every(1000, || {
        set_text("clock", &ui::ms_hms(now() as i64));
    });
    every(500, flow_tick);
    every(20_000, || {
        if let Some(ws) = WS.with(|c| c.borrow().clone()) {
            let _ = ws.send_with_str("{\"type\":\"ping\"}");
        }
    });
    every(2000, || {
        // ws watchdog: reconnect when down; degrade to polling after failures
        let (open, tries, poll) = STATE.with(|s| {
            let st = s.borrow();
            (st.ws_open, st.last_ws_try, st.poll_mode)
        });
        let wait = if poll { 10_000.0 } else { 2_500.0 };
        if !open && now() - tries > wait {
            connect_ws();
        }
    });
    every(1500, || {
        let poll = STATE.with(|s| s.borrow().poll_mode);
        if poll {
            fetch_snapshot();
            fetch_traders_rest();
            fetch_radar();
        }
    });
    every(10_000, || {
        // radar REST backup (WS pushes every 3s for the selected coin)
        let view = STATE.with(|s| s.borrow().side_view.clone());
        if view == "radar" {
            fetch_radar();
        }
    });
    every(15_000, || {
        // opportunities REST backup (WS pushes every 5s)
        fetch_opps();
    });
    every(5_000, || {
        // bookmap resync when the live socket is down
        let (open, poll, view) = STATE.with(|s| {
            let st = s.borrow();
            (st.ws_open, st.poll_mode, st.side_view.clone())
        });
        if view == "book" && (!open || poll) {
            fetch_bookmap_if_stale();
        }
    });
    every(30_000, || {
        let (open, poll) = STATE.with(|s| {
            let st = s.borrow();
            (st.ws_open, st.poll_mode)
        });
        if open && !poll {
            fetch_snapshot(); // resync retro-tagged intents
        }
    });

    // render loop
    start_render_loop();
    update_dots();
    switch_side_view("book");
    connect_ws();
    fetch_assets();
    fetch_tuning();
    fetch_opps();
}

fn start_render_loop() {
    RAF.with(|r| {
        let cb = Closure::<dyn FnMut()>::new(|| {
            let view = STATE.with(|s| s.borrow().side_view.clone());
            if view == "book" {
                if let Some(canvas) = BOOK_CANVAS.with(|c| c.borrow().clone()) {
                    STATE.with(|s| {
                        let st = s.borrow();
                        BOOK.with(|b| {
                            let mut bv = b.borrow_mut();
                            bookmap::draw(
                                &mut bv,
                                &st.tape,
                                &st.liqs,
                                &st.windows,
                                &st.radar,
                                &canvas,
                            );
                        });
                    });
                }
            } else if let Some(canvas) = CANVAS.with(|c| c.borrow().clone()) {
                STATE.with(|s| {
                    let mut st = s.borrow_mut();
                    if st.dirty || now() - st.last_draw > 250.0 {
                        canvas::draw(&st, &canvas);
                        st.dirty = false;
                        st.last_draw = now();
                    }
                });
            }
            let f = RAF.with(|r| {
                r.borrow()
                    .as_ref()
                    .map(|c| c.as_ref().unchecked_ref::<js_sys::Function>().clone())
            });
            if let Some(f) = f {
                let _ = web_sys::window().unwrap().request_animation_frame(&f);
            }
        });
        *r.borrow_mut() = Some(cb);
    });
    let f = RAF.with(|r| {
        r.borrow()
            .as_ref()
            .map(|c| c.as_ref().unchecked_ref::<js_sys::Function>().clone())
    });
    if let Some(f) = f {
        let _ = web_sys::window().unwrap().request_animation_frame(&f);
    }
}
