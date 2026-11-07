# Measurement Spec — v1 Falsification Harness

> Ingested 2026-11-07 from `context.md`. This is the engineering standard
> the harness is held to. Central rule: **the first system must be designed
> to prove itself wrong.** If its logs cannot distinguish a real opportunity
> from polling delay, feed noise, or illiquidity, it is not ready for
> execution.

## The one-line test

The project succeeds only if this statement becomes true repeatedly:

> "The authoritative source updated, my system observed it before the venue
> book repriced, the resulting quote had enough depth, and the trade would
> still have been profitable after all costs."

Until clean logs say that across many independent events, execution is
premature.

## End-to-end failure points (consolidated)

| Stage | Main problem | Why it matters |
| --- | --- | --- |
| Release scheduling | Wrong timezone, delayed/rescheduled release | You poll at the wrong time or mislabel the event |
| Origin polling | Interval too coarse; origin changes between polls | You measure your own polling delay, not the market's reaction delay |
| Origin parsing | Ambiguous, revised, preliminary, malformed data | Wrong outcome inferred; false signal |
| Clock synchronization | Mixing wall-clock and monotonic time; cross-domain comparisons | Latency measurements become fiction |
| Book feed | Missed/out-of-order/stale updates, reconnect gaps | Missed repricing or reconstructed false book state |
| Market mapping | Release linked to wrong market/outcome token | You trade an unrelated contract |
| Race detection | "Any book move" treated as the reaction | Noise/unrelated flow counted as edge |
| Liquidity | Quote exists but size tiny or vanishes | Simulated edge evaporates under real execution |
| Fees & spread | Taker fees, spread crossing, adverse selection ignored | Positive raw edge becomes negative realized edge |
| Execution | Order arrives after repricing | Opportunity gone before it lands |
| Risk | Duplicate orders, stale quotes, runaway inventory, one-sided fills | One bad event becomes a large loss |
| Operations | Silent feed failures, clock drift, disk-full, missed releases | Biased or incomplete evidence |

## 1. Polling is inherently lossy

Polling every 250 ms ⇒ average detection delay ≈ 125 ms **before** network,
parsing, processing. You can never claim to have seen a release "at the
millisecond it was published" — only within your polling interval.

Mitigations:

- Prefer server push (SSE/WebSocket/changelog endpoint) when available.
- Raise polling frequency only in a narrow pre-release window, not
  continuously.
- Record request-sent and response-received times with a monotonic clock.
- Record HTTP `Date`, `Last-Modified`, `ETag`, and any source-provided
  publish timestamp.
- Treat "origin seen" as **your detection time**, not the publication time;
  the first observed update is an *upper bound* on publication time.

**Publication uncertainty window.** With a monotonic clock:

- `t_prev_req_sent` = send time of the last poll that returned the OLD state
- `t_curr_headers_rx` = first-byte time of the poll that returned the NEW state
- true mutation `t_pub ∈ [t_prev_req_sent, t_curr_headers_rx]`
- log `W_uncertainty = t_curr_headers_rx − t_prev_req_sent`

Conservative analysis assumes the origin changed at the *start* of the
window. Example: if `W_uncertainty = 300 ms` and the book reprices 80 ms
after headers, you were relatively slower than you thought. Never claim
"the market was slow" unless you can bound when the source actually changed.

## 2. The origin is harder than it looks

Traps: cached CDN responses returning old values; renamed JSON keys / locale
formatting; preliminary figures later revised; multiple pages/endpoints
updating at different times; anti-bot throttling **during exactly the
release window**; ambiguous resolution rules that don't map cleanly to the
published number. Also: seasonally adjusted vs unadjusted; headline vs core
vs revision vs YoY vs MoM; rounding/format changes; page updated before the
official headline exists; a release that doesn't actually resolve the
market's stated rules.

A release is only useful if it resolves the market **unambiguously**. Build
a validation layer: parse the value, compare against the market's resolution
criteria, and mark uncertain cases `no_signal` rather than forcing a trade.

## 3. Timestamp fidelity (can quietly invalidate the study)

Three clock domains, never mixed:

- **Local monotonic** — all latency differences inside the process.
- **Local UTC wall** — correlation/logging only; subject to NTP adjustments.
- **Exchange/feed timestamps** — context only; own skew and granularity.

Core race measurement:
`reaction_latency = t_book_change − t_origin_detected`, both captured by the
same process on the same monotonic clock. Do not convert this into an
absolute "beat the market by N ms" claim without separately validating
clock alignment.

Timestamp every stage separately: poll scheduled → request sent → response
headers received → response body received → parse completed → signal
generated → book update observed → simulated order decision. Without these
breakpoints you cannot tell where time was lost.

## 4. Book-feed integrity

WebSockets drop, reconnect, miss messages, reorder updates, or deliver stale
snapshots after recovery. Requirements: sequence-number validation where the
venue provides it; snapshot-plus-delta reconciliation; explicit
reconnect/resync logic; a **book-healthy flag** (never generate signals from
a book you cannot verify); gap markers in the log whenever the feed is
interrupted. Otherwise a reconnect can look like a repricing — or hide one.

## 5. "Book changed" ≠ "market reacted"

Bad definition: "any change in best bid or ask."
Better: "a material change in the relevant outcome's executable price,
beyond a predeclared threshold, with sufficient displayed depth."

Pre-declare **before collecting data**: which price (best bid/ask/mid/
executable VWAP); the reprice threshold; which side matters for your
intended order; minimum depth; how long the quote must persist. Tuning these
after seeing data is overfitting.

## 6. Displayed liquidity is not executable liquidity

The most expensive mistake: treating a stale top-of-book quote as a
guaranteed fill. At release time: other bots cancel/replace; price moves
before your order lands; displayed size may be reserved/stale/split across
levels; market orders cross multiple levels; limit orders may not fill.

Dry-run must model **conservative fills**: price after a hypothetical order
latency; depth consumed across multiple levels (VWAP); fees; configurable
execution delay; a "no fill" outcome when the quote is gone; partial fills.
Report optimistic AND conservative fills — if the edge exists only in the
optimistic case, it is probably not an edge. Track **phantom edge**:
opportunities visible in the log but not executable in replay.

## 7. Adverse selection is the real enemy

Detecting first doesn't mean trading against dumb money: the release may be
ambiguous while sophisticated participants already priced the likely
interpretation. Buying the "correct" side can mean providing liquidity to
someone who knows the resolution rules better than you do.

Protections: strict market-resolution specification before trading;
confidence threshold for parsed values; blacklist ambiguous/disputed
releases; position limits per market and per release; kill switch on
uncertain feed health, wallet balance, or market state.

## 8. Rust helps, but does not make the system fast

The latency budget is dominated by: network RTTs to origin and venue; TLS
handshakes and connection reuse; OS scheduling; (de)serialization; logging
I/O; matching-engine latency; geographic distance. A Rust binary opening a
fresh TLS connection at release time loses to a warm Python process with
persistent connections. Optimize the whole path, not the language.

## 9. Logging must not sit in the hot path

Synchronous disk writes during a burst add latency exactly when it matters.
Use a bounded non-blocking channel from hot path to logger; compact format;
buffered writes with explicit flush policy; separate hot-path capture from
analysis; backpressure = drop-and-count, never stall the feed handler.

Silent bias is the biggest operational risk: if the logger fails during
exactly the fastest releases, evidence is systematically biased. Require
heartbeats (feed health, polling liveness, disk space); explicit
release-completeness records (expected / observed / missed / parse-failure);
separate raw-payload, parsed-event, decision, and error logs; replayability
from raw events; versioned market mappings and thresholds. **If you only log
successful races, you will convince yourself the system works.**

## 10. Market discovery may dominate

Weak markets: thin books, wide spreads, low volume, ambiguous resolution,
few recurring releases, prices already efficient. Better candidates:
recurring unambiguous releases, liquid books, measurable historical reaction
behavior. Include a market-quality score, not just keyword matches.

## Version gates

- **v1** — detect, timestamp, log, replay, analyze (this repo today).
- **v1.5** — validate edge across many releases and markets.
- **v2** — pre-sign and execute only high-confidence opportunities.
- **v3** — production risk controls, reconciliation, alerting, capital
  management.

Building execution before the data justifies it is the biggest workflow
inefficiency. Pre-signing reduces signing latency but does **not** solve
network transit, matcher latency, price movement before inclusion,
rejection/partial fills, nonce/allowance/balance problems — and a
pre-signed order must never bypass position limits, market-status checks, or
a kill switch.

## Post-signal forward-walk protocol (kills phantom edge)

At `t_signal` (parser confirms a valid signal) do NOT credit yourself the
book at `t_signal`. Keep consuming the live feed and snapshot the local book
at fixed offsets: **+0, +25, +50, +100, +250, +500 ms**. At each offset,
simulate sweeping the book for the target notional ($10) across multiple
levels (multi-level VWAP + fees).

Verdicts:

- Profitable at +0 ms but the ask jumps to ~$0.99 / depth → $0 by +50 ms ⇒
  `PHANTOM_EDGE` (the MM cancel was already in flight or beat your transit).
- Net profit persists at **+100 ms (conservative execution delay)** and the
  book was continuously healthy ⇒ `EXECUTABLE_EDGE`.

## Pre-declared spec (anti-overfitting)

The harness must lock the definition of "market reacted" and "valid signal"
**before** the release window opens. The source uses `spec.toml`; cashcat
expresses the same fields in the `watch --config` JSON:

| Field | Source example | Meaning |
| --- | --- | --- |
| `spec_version`, `event_id`, `expected_release_utc` | `tsa_daily_2025_05_12`, 13:00:00Z | release identity & schedule |
| `poll_window_start/end_offset_sec` | −120 / +300 | when to poll intensively |
| `active_poll_interval_ms` | 100 | in-window cadence |
| `condition_id`, `yes_token_id`, `no_token_id` | 0x… / ERC-1155 ids | exact market mapping |
| `resolution_threshold` | 2,500,000 passengers | value ⇒ YES above |
| `allow_preliminary` | false | reject flagged-preliminary numbers |
| `reprice_price_threshold_ticks` | 920 (= $0.920; 1 tick = $0.001) | reaction threshold |
| `min_displayed_depth_usd` | 25.0 | depth required at the quote |
| `persistence_duration_ms` | 500 | quote must hold this long (ignores flickers) |
| `target_notional_usd` | 10.0 | simulated size |
| `taker_fee_bps` | 0 (adjust per market schedule) | fee model |
| `conservative_exec_delay_ms` | 100 | edge must survive this delay |

## Origin probe requirements

Instrument every poll in ≥ 4 monotonic breakpoints (scheduled / req sent /
headers rx / body rx / parse done) and capture CDN state: `CF-Cache-Status`
or `X-Cache`, and `Age` — an `Age > 0` / `HIT` during the release window
means a CDN served you a cached ghost response and the measurement is void.

## Book model requirements

Fixed-point integer levels (e.g. `BTreeMap<u32 ticks, u64 micro-shares>`,
1 tick = $0.001) — no float ordering bugs. Health state machine:
`Uninitialized / Healthy / SequenceGapDetected / Disconnected / StaleFeed`.
Sweep simulation walks levels ascending, stops at a cap price (e.g. never
sweep ≥ $0.98 — no edge remains after risk/fees), and values each winning
share at $1.00 redemption.

## v1 → v1.5 go/no-go scorecard (per market type, 10–20 releases)

| Metric | Fail (kill market / fix feed) | Pass (advance toward v2) |
| --- | --- | --- |
| Completeness rate | < 95% of scheduled releases captured cleanly | 100% captured with book Healthy |
| CDN cache poisoning | `Age > 0` or `HIT` during release window | `DYNAMIC`/`MISS`/`BYPASS` verified |
| Uncertainty bound `W_uncertainty` | ≥ reaction latency (polling too slow to prove edge) | < 25% of reaction latency |
| Ambiguity / revision rate | > 0% revisions that flip the outcome | 0% ambiguity; 1-to-1 deterministic resolution |
| Phantom-edge decay | Profitable at +0 ms but EV ≤ $0 at +100 ms | Net profit > $1 on $10 size persists at +100 ms and +250 ms |
| Confirmed reaction latency | < 150 ms (MMs already on the raw feed) | > 400 ms with persistent depth ≥ $25 |

## Immediate next steps (per source)

1. Pick ONE recurring daily release market.
2. Write its spec; implement strict payload validation with `no_signal`
   guards (preliminary flags, schema mismatch, revision detection).
3. Run the harness for 5 consecutive releases and inspect the forward-walk
   decay curve from +0 ms to +500 ms.

## Metrics to make first-class before any execution code

origin detection-lag distribution; parse success & ambiguity rate; book-feed
gap/reconnect rate; time from origin detection to first qualifying book
change; quote persistence duration; simulated executable edge after fees and
slippage; false-positive rate; data completeness per release; system uptime
and missed-release count.