# Execution Reference (v2 — DEFERRED, DO NOT BUILD YET)

> Ingested 2026-11-07 from `context.md`. Everything here is **reference
> material only**. Per `MEASUREMENT-SPEC.md` version gates, no signing,
> wallet, or order-placement code belongs in this repo until the go/no-go
> scorecard passes across 10–20 releases. Some operational claims below
> (fee schedule, geo-blocking, region) come from the source and need
> first-hand verification before any money moves.

## 1. The 3-second live-sports taker delay (the amateur trap)

Markets flagged `seconds_delay: 3` (most major live traditional sports):
taker orders are frozen in a matching-engine buffer for 3,000 ms before
execution — enough time for MMs to cancel stale quotes. The flag is a
per-market field (`seconds_delay` / `fpmmLive`) in the Gamma/CLOB market
docs. **Filter: exclusively `seconds_delay == 0`** (instant matching).
`cashcat list --zero-delay` implements this scanner.

Best zero-delay categories: live speech/transcript markets; real-time API
counters & telemetry; rapid crypto micro-resolution (hourly Binance candle
markets); unflagged esports / niche live events (see `BLUEPRINT.md`).

## 2. Endpoints & topology

- Matching engine: `https://clob.polymarket.com` — behind Cloudflare,
  reportedly routing to AWS **eu-west-2 (London)**. Co-locate there
  (free-tier `t3.micro` / `t4g.small` ⇒ ~1.5 ms RTT).
- Order submission: `POST https://clob.polymarket.com/order`.
- Keep-warm heartbeat: `GET https://clob.polymarket.com/time` every ~2.5–3 s
  (keeps TCP congestion window open, TLS session hot; a fresh TLS handshake
  costs ~3 RTTs ≈ 5–30 ms).
- Book feed: `wss://ws-subscriptions-clob.polymarket.com/ws/market`.
- Market metadata & descriptions: `https://gamma-api.polymarket.com/markets`
  / `/events?active=true&closed=false&limit=100`.

## 3. L2 API authentication (HMAC)

Credentials derived once via `createApiKey()` (wallet-signed):
`POLY_API_KEY`, `POLY_SECRET`, `POLY_PASSPHRASE`.

Every `POST /order` needs headers: `POLY_ADDRESS`, `POLY_API_KEY`,
`POLY_PASSPHRASE`, `POLY_TIMESTAMP`, `POLY_SIGNATURE`,
`Content-Type: application/json`.

`POLY_SIGNATURE` = **HMAC-SHA256** over
`timestamp + method + path + body` (e.g. `ts + "POST" + "/order" + json`),
base64 **URL_SAFE** encoded. The exchange accepts the timestamp within a
multi-second window, so the exact HTTP body **and** HMAC header can be
pre-computed once per second (for `t` and `t+1`) by a background thread and
stored in an `ArcSwap<PreBuiltRequest>`.

## 4. Zero-compute pre-signing

- Every order also needs an EIP-712 signature (Keccak256 + secp256k1) —
  ~15 ms if done after seeing the event; that loses races.
- At startup (and whenever bankroll changes / every ~2 s), build and sign
  BUY YES and BUY NO limit orders at $0.92–$0.95 in RAM. Keep the raw
  signed hex blobs hot.
- Trigger path (< 50 µs): origin parser confirms outcome → load pointer
  from `ArcSwap` → write pre-computed bytes into the warm HTTP/2 stream.
  Zero cryptography, zero JSON serialization.

## 5. Order-type discipline

- **FAK / Immediate-Or-Cancel** sweeps only: sweep whatever cheap liquidity
  remains up to the cap price, cancel the rest in 0 ms. Never GTC — a late
  GTC order rests on the book and can be dumped on if the signal was a
  false alarm or gets revised.
- Cap price ~$0.95 buys every share ≤ $0.95, guaranteeing ≥ 5% gross if the
  outcome holds; sell at $0.98 immediately rather than holding to $1.00.
- Pre-fire check (free — CLOB API calls are off-chain HTTP):
  `if current_best_ask > 0.92 { return; }` — if the cheap liquidity is
  already gone, don't fire; cost $0.
- FOK mentioned equivalently for all-or-nothing entries.

## 6. Market eligibility rule

Read the market `description` from gamma-api before arming anything. Only
target markets naming an exact, deterministic digital source ("official
number published at tsa.gov/…", "official @elonmusk X timeline", "Binance
BTC/USDT 1m candle close"). **Never** automate subjective markets where
humans argue over definitions (oracle-dispute risk vs primary-source
resolution).

## 7. Money plumbing (when, not if)

- Bridge $10 → Polygon PoS → USDC(.e) → deposit to Polymarket CTF
  Exchange; keep ~$0.50 POL for gas (one-time approval ≈ $0.10).
- Start size $2–5; scale only on wins; off-chain order placement costs
  nothing when unfilled.
- US geo-blocking: CLOB requires a non-US exit IP (the co-located box
  serves double duty).
- Fees claimed at 2% of winnings ⇒ edge must clear ~3%; prefer buys < $0.85.
- Rust deps cited by the source: `alloy` (signers/sol-types), `hmac`,
  `sha2`, `base64`, `arc-swap`, `tokio-tungstenite`, `reqwest`.

## 8. The three ways a bot loses money (and the coded immunities)

1. **Late entry into repriced book** → FAK + best-ask gate (§5).
2. **Resting order run over after a false/revised signal** → never GTC;
   self-destruct unfilled remainder (§5).
3. **Ambiguous resolution** → §6 eligibility rule + `no_signal` parsing.

Independent checks must run immediately before submission regardless of
pre-signing; a pre-signed order must never bypass position limits,
market-status checks, or the kill switch.

## 9. Engine sketch (for future implementation, not built)

```rust
pub struct ArmedPayload {          // fully serialized, ready to send
    pub body_bytes: bytes::Bytes,  // pre-signed EIP-712 order JSON
    pub poly_timestamp: String,
    pub poly_hmac_sig: String,
}
// SniperEngine holds:
//   http_client (warm HTTP/2 pool to clob.polymarket.com),
//   armed_yes / armed_no: Arc<ArcSwap<ArmedPayload>>,
//   api_key / api_secret / passphrase / wallet_address
// Background loop every 500 ms:
//   recompute HMACs for current second (YES + NO) -> store into ArcSwap
//   every ~2.5 s: GET /time to keep the socket warm
// Hot path on signal:
//   payload = (yes ? armed_yes : armed_no).load()
//   POST /order with POLY_* headers + payload.body_bytes  (no awaits on crypto)
```

## 10. Deployment checklist (source's 48-hour plan)

1. Zero-delay scanner picks 2 high-frequency markets resolving today or
   tomorrow, sorted by 24 h volume.
2. Reverse-engineer each origin in DevTools (Network → WS/XHR); confirm
   ETag/Last-Modified support via `curl -I`.
3. Dry-run telemetry race until logs print e.g.
   `Origin detected at T+0ms | Ask $0.68 ($450 available) | repriced to
   $0.99 at T+1,840ms | Net Edge Window: +1,840ms`.
4. Only after the scorecard passes: fund, approve, arm at $2, FAK-sweep,
   exit at $0.98.

## 11. Pivot rule

If measured Polymarket windows are consistently < 300 ms, the venue is too
efficient for this stack — same system against **Limitless (Base)** or
**Azuro**, claimed 10× less competition.