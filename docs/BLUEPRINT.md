# Blueprint — Deterministic Reality Arbitrage ("Betting on the Past")

> Ingested 2026-11-07 from `context.md` (raw source dump). This file is the
> **strategy thesis**. Measurement discipline lives in `MEASUREMENT-SPEC.md`;
> deferred execution mechanics live in `EXECUTION-REFERENCE.md`. Numeric
> claims below are from the source material and are **claims to verify**,
> not measurements — that is exactly what the v1 harness exists to do.

## Thesis

Turn a tiny bankroll ($10) into $10,000 without flash loans by exploiting the
latency between real-world data sources and crypto prediction markets
(Polymarket on Polygon; also Azuro, Overtime, Limitless on Base).

- Polymarket lists hyper-specific real-world markets: Fed decisions, Elon
  Musk tweet counts, daily TSA checkpoint numbers, esports match winners.
- Every market's rules name a **Resolution Source** (e.g. tsa.gov, the Fed
  RSS feed, Riot Games API, X API).
- **The human flaw:** humans refresh webpages or wait for a reporter to
  tweet, then manually click Buy. That takes 5–30 seconds.
- **The edge:** scrape the raw JSON endpoint / backend API of the exact
  resolving source. The instant data posts to the origin server — before the
  webpage renders for human eyes — buy the now-winning shares at $0.70–$0.85.
  Seconds later the market bids them to $0.99.
- You are not predicting the future; you are betting on an event that already
  happened, during the window before other participants realize it.

## What an edge actually is

> Edge = E[Profit | Signal] − Fees − Slippage − Adverse Selection > 0 over
> 1000+ trials where YOU are structurally forced to win because the
> counterparty is structurally forced to be slow, blind, or dumb.

Only four edges exist in automation:

1. **Speed** — being closer to the server (too expensive for us)
2. **Capital** — moving price (we don't have it)
3. **Computation** — solving math others can't within 100 ms (free)
4. **Information** — knowing the outcome 500 ms before their API tells them
   (free) ← **this project targets #4**

Polymarket / Azuro / Limitless are claimed to be well-suited because their
market makers are "JavaScript bots polling a slow aggregator."

## The Dead-Man's Window

Market makers (MMs) post resting limit orders on Polymarket's CLOB. When a
deterministic real-world event occurs, MMs must cancel stale orders — but
they watch slow third-party aggregators or webhooks. Claimed window between
"event happens on the origin server" and "MM's `cancelOrders()` hits the
exchange": **500 ms – 4,000 ms**.

## Target selection (claimed to be 90% of the edge)

**Avoid:** big political/macro markets (presidential election, Fed) —
"infested with Jane Street bots."

**Prefer high-frequency, low-sexy, API-resolvable markets:**

| Category | Resolution origin | Notes |
| --- | --- | --- |
| Esports micro-markets (CS2/Dota 2/LoL map winners) | GRID/Bayes raw WebSocket, HLTV | NOT the Twitch stream (15 s video delay) |
| Tennis point-by-point | Sportradar / official ATP firehose | |
| Daily recurring data | `tsa.gov` passenger numbers (~9 am ET), crypto ETF inflow postings (Farside/issuer JSON), weather.gov API, hurricane NHC advisories, Box Office Mojo daily drops | |
| API counters | @elonmusk tweet-count bins | |
| Crypto micro-resolution | "BTC above $X at [Hour]:00 UTC on Binance" — resolves on exact 1m kline close; in the final ~800 ms the Binance aggTrade WS mathematically locks the candle while retail orders sit mispriced at $0.40–$0.70 | |
| Live speech / transcript markets | Live-captioning WebSocket feeds (Cart/StreamText) run 4–8 s **ahead** of YouTube HLS (8–15 s encoding delay); match with whisper.cpp or regex | "What words will Powell say…", keynote announcements |
| Unflagged niche live events | Secondary esports, chess (Chess.com/Lichess relay sockets) listed without the 3-second taker delay | scanner flags `seconds_delay == 0` |

## Two-plane architecture (endgame)

```
[PLANE 1: ORIGIN SCRAPER] -> [DECISION ENGINE < 0.3 ms] -> [PLANE 2: EXECUTION]
        (Rust)                       (Rust, in RAM)             (pre-signed)
```

- **Plane 1 (the moat):** find the REAL endpoint via DevTools
  (Network → XHR/WS) — the JSON that updates 2–4 s before the UI renders.
  Warm HTTP/2 (or QUIC) connection with TLS session resumption; conditional
  requests (`If-None-Match` / `If-Modified-Since`) so you receive tiny
  `304 Not Modified` responses until the exact millisecond the payload drops
  (claimed to beat Cloudflare cache purge by ~400 ms). Co-locate near the
  exchange and the origin (claims: home ping ~80 ms → ~0.8 ms on a $5
  Hetzner box; ~1.5 ms RTT from AWS `eu-west-2`, where the CLOB matching
  engine reportedly routes behind Cloudflare).
- **Plane 2 (wins races):** every Polymarket order needs an EIP-712
  signature; signing after seeing the event costs ~15 ms. Pre-sign BUY YES
  and BUY NO orders in RAM before the event; on trigger, dump pre-signed raw
  bytes down an already-warm socket. No crypto, no JSON building on the hot
  path. Details in `EXECUTION-REFERENCE.md`.

## The "can't lose" claim (and its conditions)

The source argues principal is safe because:

- If you're first: buy YES @ $0.70 now worth $1.00; sell at $0.98 or hold to
  $1.00 settlement (+30–40% in ~5 s).
- If a faster bot beat you: your order simply doesn't fill, you pay $0 and
  cancel; Polygon gas ~$0.001. Collateral never leaves the wallet unless you
  get a profitable fill. Use FOK/FAK orders, never resting orders.
- Compounding math (claim): 25% per event × 20 events turns $10 → $867;
  × 40 events → $75,231 (`10 × 1.25^40`).

**This holds only if** measurement is valid, liquidity is executable, and
resolution is unambiguous — see `MEASUREMENT-SPEC.md` for every way it
silently fails.

## Pro-tips from the source

- Polymarket **geo-blocks US clients** on the CLOB; a non-US exit IP is
  required (co-located server doubles as the exit).
- Fees: claimed **2% on winnings only** → edge must exceed ~3% to matter;
  prefer buys below $0.85.
- Do not poll government sites every 100 ms from one IP — Cloudflare ban in
  ~10 minutes. Use conditional headers (+ rotating residential proxies if it
  ever comes to that; v1 deliberately stays single-IP and polite).
- MMs are "dumbest" on **Limitless (Base) and Azuro**; Polymarket is getting
  sharper. If the measured Polymarket window is < 300 ms, pivot venues —
  same logic, claimed 10× less competition.
- Don't build a generic bot; build a sniper for ONE API that 99% of traders
  are too lazy to reverse-engineer.

## Origin 48-hour plan (for context)

1. **Day 1 — measure the window, no money:** pick ONE market type (CS2 map
   winner or daily TSA numbers: daily cadence, $30k–$200k volume, little HFT
   competition). Dry-run binary logs `T_origin_arrival` vs the book's
   reprice; consistently > 800 ms windows = a candidate edge.
2. **Day 2 — wire execution:** USDC on Polygon (~$0.50 POL for gas), API
   creds via `createApiKey()`, go live at $2, sell at $0.98, don't hold to
   $1.00.

This repo implements Day 1 properly (and per `MEASUREMENT-SPEC.md`, Day 2 is
gated behind the go/no-go scorecard, not a calendar).