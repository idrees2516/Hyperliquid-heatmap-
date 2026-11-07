//! Hyperliquid wire types (REST + WebSocket).

use serde::{Deserialize, Serialize};
use shared::{Candle, Side};

// ---------------------------------------------------------------------------
// REST responses
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct HlMeta {
    pub universe: Vec<HlMetaAsset>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HlMetaAsset {
    pub name: String,
    #[serde(default)]
    pub sz_decimals: u32,
    #[serde(default)]
    pub max_leverage: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HlAssetCtx {
    #[serde(default)]
    pub funding: String,
    #[serde(default)]
    pub open_interest: String,
    #[serde(default)]
    pub prev_day_px: String,
    #[serde(default)]
    pub day_ntl_vlm: String,
    #[serde(default)]
    pub mark_px: String,
    #[serde(default)]
    pub oracle_px: String,
}

#[derive(Debug, Deserialize)]
pub struct HlCandle {
    pub t: i64,
    #[serde(default)]
    pub s: String,
    pub o: String,
    pub c: String,
    pub h: String,
    pub l: String,
    pub v: String,
    #[serde(default)]
    pub n: i64,
}

impl HlCandle {
    pub fn to_candle(&self) -> Option<Candle> {
        Some(Candle {
            t: self.t,
            o: self.o.parse().ok()?,
            h: self.h.parse().ok()?,
            l: self.l.parse().ok()?,
            c: self.c.parse().ok()?,
            v: self.v.parse().ok()?,
            n: self.n,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HlUserFill {
    pub coin: String,
    pub dir: String,
    pub px: String,
    pub sz: String,
    pub time: i64,
    #[serde(default)]
    pub closed_pnl: String,
    #[serde(default)]
    pub start_position: String,
    #[serde(default)]
    pub crossed: Option<bool>,
    #[serde(default)]
    pub fee: String,
    #[serde(default)]
    pub liquidation: Option<serde_json::Value>,
    #[serde(default)]
    pub tid: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HlClearinghouseState {
    #[serde(default)]
    pub margin_summary: HlMarginSummary,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HlMarginSummary {
    #[serde(default, rename = "accountValue")]
    pub account_value: String,
    #[serde(default, rename = "totalNtlPos")]
    pub total_ntl_pos: String,
}

// ---------------------------------------------------------------------------
// WebSocket messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct HlWsTrade {
    pub coin: String,
    /// "A" = taker sold (hit bid), "B" = taker bought (lifted ask)
    pub side: String,
    pub px: String,
    pub sz: String,
    pub time: i64,
    #[serde(default)]
    pub tid: serde_json::Value,
    /// users[0] = taker, users[1] = maker
    #[serde(default)]
    pub users: Vec<String>,
}

impl HlWsTrade {
    pub fn side(&self) -> Option<Side> {
        match self.side.as_str() {
            "B" | "b" => Some(Side::B),
            "A" | "a" => Some(Side::A),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HlWsLiquidation {
    pub user: String,
    #[serde(default, rename = "l")]
    pub liqs: Vec<HlLiqInner>,
}

/// One level in an `l2Book` snapshot.
#[derive(Debug, Clone, Deserialize)]
pub struct HlBookLevel {
    pub px: String,
    pub sz: String,
    /// order count — number or string depending on asset
    #[serde(default)]
    pub n: serde_json::Value,
}

/// Full `l2Book` snapshot frame.
#[derive(Debug, Clone, Deserialize)]
pub struct HlBook {
    pub coin: String,
    /// upstream book time — HL intermittently returns `null` here
    /// (observed on both REST and WS frames); callers fall back to the
    /// local wall clock
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub time: Option<i64>,
    pub levels: HlBookSides,
}

/// tolerant i64 decoder: numbers, numeric strings and null all accepted
fn de_opt_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    use serde::de::Error;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => Ok(n.as_i64()),
        serde_json::Value::String(s) => s.parse::<i64>().ok().map(Some).ok_or_else(|| {
            D::Error::custom(format!("invalid book time string: {s}"))
        }),
        other => Err(D::Error::custom(format!("invalid book time: {other}"))),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HlBookSides {
    #[serde(default)]
    pub bids: Vec<HlBookLevel>,
    #[serde(default)]
    pub asks: Vec<HlBookLevel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HlLiqInner {
    pub coin: String,
    #[serde(default)]
    pub szi: String,
    #[serde(default)]
    pub px: String,
    #[serde(default, rename = "side")]
    pub side: String,
    #[serde(default)]
    pub mark_px: Option<String>,
}

/// Generic envelope: { "channel": "...", "data": ... }
#[derive(Debug, Deserialize)]
pub struct WsEnvelope {
    pub channel: String,
    pub data: serde_json::Value,
}

/// Subscribe / unsubscribe request
#[derive(Debug, Serialize, Clone)]
pub struct WsSub {
    pub method: &'static str,
    pub subscription: serde_json::Value,
}

pub fn sub_trades(coin: &str) -> WsSub {
    WsSub {
        method: "subscribe",
        subscription: serde_json::json!({ "type": "trades", "coin": coin }),
    }
}

pub fn sub_candle(coin: &str) -> WsSub {
    WsSub {
        method: "subscribe",
        subscription: serde_json::json!({ "type": "candle", "coin": coin, "interval": "1m" }),
    }
}

pub fn sub_liq() -> WsSub {
    WsSub {
        method: "subscribe",
        subscription: serde_json::json!({ "type": "wLiquidation" }),
    }
}

pub fn sub_delev() -> WsSub {
    WsSub {
        method: "subscribe",
        subscription: serde_json::json!({ "type": "wDeleveraging" }),
    }
}
