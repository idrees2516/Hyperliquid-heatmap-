# ⚡ Onchain Footprint

**A new charting standard for Hyperliquid perps — built entirely in Rust, powered by all available on-chain data.**

![demo](docs/demo.gif)

Onchain Footprint is a realtime order-flow terminal that lets you **look inside every candle across all Hyperliquid perpetuals**. Every trade on Hyperliquid is fully on-chain and attributed to wallets, so the footprint shows *who* is buying and selling, *why* (intent), and *whether those traders are winners or losers*.

- **Footprint chart** — per price level inside every candle: taker **SELL × BUY** notional (USD), heat-mapped, with the point-of-control outlined and a per-window delta strip.
- **Who is trading** — every print carries the **taker and maker wallet** (link straight to Hypurrscan). A 60-minute trader leaderboard ranks wallets by flow with per-wallet buy/sell split and market share.
- **Intent behind each trade** — fills are classified live as `AGGR` (aggressive open/increase), `LIQ` (liquidation / deleveraging), `STOP` (realized-loss reduce), `TP` (realized-profit reduce), `HEDGE` (cross-market delta-neutral leg) or `REDUCE`.
- **Winners & losers** — wallets are enriched via on-chain `userFills`: realized PnL, win rate, account value, and a conviction tag (`++ + ± - --`).
- **Trading style** — behavioral + enriched classification: `MM` (resting/maker-dominated), `SCALPER`, `INTRADAY`, `DAY`, `SWING`, `POSITION`.
- **Level drill-down** — click any footprint cell (or candle) to open the wallets that traded that exact price bucket in that exact window: bought/sold, session and all-time PnL, win rate, open/increase/reduce/flip split, liquidation flag.
- **Realtime** — trades, candles and liquidation events stream from Hyperliquid's L1 over WebSocket; the engine folds every print into the live candle as it prints.

## Stack

Pure Rust, frontend and backend:

```
Hyperliquid L1 (api.hyperliquid.xyz: info REST + wss realtime)
        │
        ▼
┌────────────────────────────────────────────────────────┐
│ backend  (axum + tokio)                                │
│  · upstream WS connector (trades+candles, auto-rejoin)  │
│  · wallet registry: live position tracking, realized    │
│    PnL, hedge matching, intent engine                  │
│  · enrichment: userFills + clearinghouseState polling   │
│  · footprint / level-detail aggregation on demand        │
│  · browser WS fan-out + JSON REST                       │
└───────────────┬────────────────────────────────────────┘
                │  REST /api/* + WS /ws (JSON)
                ▼
┌────────────────────────────────────────────────────────┐
│ frontend (Rust → wasm32 via wasm-bindgen / web-sys)    │
│  · canvas footprint engine (candles + level cells,     │
│    POC, intent bars, delta strip, crosshair, LIVE tag)  │
│  · realtime tape / liquidations / trader leaderboard    │
│  · level drill-down modal                               │
└────────────────────────────────────────────────────────┘
```

Workspace layout:

| crate      | what it is                                                       |
|------------|------------------------------------------------------------------|
| `shared`   | wasm-safe types + footprint bucketing math (used by both sides)  |
| `backend`  | axum server, Hyperliquid connector, intent + trader engine       |
| `frontend` | wasm-bindgen / web-sys terminal (canvas + DOM), built with trunk|

## Run it

Requires: Rust (stable) with the `wasm32-unknown-unknown` target and [trunk].

```bash
# build the frontend (wasm -> frontend/dist)
cd frontend && trunk build && cd ..

# run the backend (serves the UI on http://localhost:3000)
cargo run --release -p backend
```

Then open <http://localhost:3000>. Use the market dropdown to switch coins and the `15s / 1m / 5m / 15m / 1h` buttons to change the footprint window. Click a level cell (or any candle) for the wallet drill-down.

[trunk]: https://trunkrs.dev

## How the intent engine works

Hyperliquid's realtime trade stream attributes each fill to its **taker and maker wallets**, and the info API exposes per-wallet `userFills` (with `closedPnl`, `startPosition`, and a `liquidation` marker) and `clearinghouseState`. Onchain Footprint combines three signals:

1. **Live position tracking** — every taker fill updates a per-wallet, per-coin position, yielding the fill's action (`open / increase / reduce / flip`) and realized PnL.
2. **Liquidation events** — `userEvents` for the largest tracked position holders flag force-closures directly; plus a behavioral detector: an aggressive full close of a losing position above a notional floor is tagged as a liquidation, and the whole burst is retro-tagged.
3. **Hedge matching** — a fill that offsets a recent opposite-direction fill in a *different* coin with similar notional is tagged as a delta-neutral `HEDGE` leg.

Reductions that realized a loss are `STOP`s, reductions that realized a profit are `TP`s, everything else is plain `AGGR` or `REDUCE`.

## Data sources & limitations

- All market data comes from Hyperliquid's official on-chain API (`api.hyperliquid.xyz`). The engine aggregates trades from the moment it starts (plus recent-trades backfill and candle history); historical footprints before process start are candle-only.
- Wallet enrichment polls the most active ~24 wallets; conviction/PnL stats are exact from the API, style tags are behavioral estimates.
- Not financial advice. Unofficial project, not affiliated with Hyperliquid.

## License

MIT
