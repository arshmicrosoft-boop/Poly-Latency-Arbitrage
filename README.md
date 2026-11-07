# cashcat

Dry-run latency-arbitrage **measurement harness** for Polymarket data-release
markets. **Read-only by design**: it subscribes to the public CLOB WebSocket
book feed and polls public origin endpoints, then logs JSONL measuring whether
the order book moves after the origin data changes, and how fast.

There is **no wallet, no signing, and no order-placement code path**. If the
logs never show a tradeable window, that is the answer, and nothing here
should grow an execution path.

## Layout

```
crates/clob     read-only Polymarket client: /sampling-markets discovery,
                /book snapshots, WSS market-channel feed (book, price_change)
crates/feeds    HttpPoller: conditional-GET polling (ETag/If-Modified-Since)
                with body-SHA256 fallback; 200 ms interval floor
crates/engine   cashcat binary: `list` (market discovery) and `watch`
                (race logger: origin flip -> book move latency)
configs/        per-experiment watch configs (see coral-hawaii.json)
data/           JSONL session logs (gitignored)
```

## Usage

```bash
# Discover active markets; --url-only keeps API-resolvable candidates
cargo run -p cashcat -- list --max-pages 8 --url-only
cargo run -p cashcat -- list --regex "(?i)(noaa|tsa|box office)"

# Instant-matching markets only (seconds_delay == 0; delayed markets are not
# snipeable — docs/EXECUTION-REFERENCE.md §1). Combine with --url-only to get
# the actionable shortlist.
cargo run -p cashcat -- list --zero-delay --url-only

# Run the race logger (Ctrl-C to stop; log lands in data/race-<ts>.jsonl)
cargo run -p cashcat -- watch --config configs/coral-hawaii.json
```

## Docs (ingested from `docs/context.md`, 2026-11-07)

| file | contents |
| --- | --- |
| [`docs/BLUEPRINT.md`](docs/BLUEPRINT.md) | strategy thesis: Dead-Man's Window, target categories, two-plane architecture |
| [`docs/MEASUREMENT-SPEC.md`](docs/MEASUREMENT-SPEC.md) | v1 falsification standard: uncertainty windows, book-health gating, forward-walk protocol, go/no-go scorecard |
| [`docs/EXECUTION-REFERENCE.md`](docs/EXECUTION-REFERENCE.md) | deferred v2 mechanics (DO NOT BUILD until the scorecard passes) |
| [`docs/context.md`](docs/context.md) | raw source dump the three docs above were ingested from (kept verbatim) |

### Config format (`watch --config`)

```json
{
  "window_s": 60,        // how long to wait for a book move after origin flip
  "heartbeat_s": 30,
  "data_dir": "data",
  // Pre-declared spec fields (docs/MEASUREMENT-SPEC.md §5/§6, anti-overfitting):
  "target_notional_usd": 10.0,  // simulated sweep size
  "cap_price": 0.95,            // never sweep above this
  "taker_fee_bps": 0,           // fee on winnings (source claims 2% = 200 bps)
  "min_depth_usd": 25.0,        // depth required at the quote for a "material" move
  "min_profit_usd": 1.0,        // net_if_win needed at +100/+250ms for EXECUTABLE_EDGE
  "book_stale_after_s": 120,    // healthy book with no updates for this long → StaleFeed
  "targets": [{
    "id": "human-readable-id",
    "token_id": "<CLOB ERC-1155 token id of the outcome to track>",
    "note": "resolution criteria, cadence, expectations",
    "origin": { "url": "https://...", "interval_ms": 5000 }
  }]
}
```

### JSONL event kinds

| kind            | meaning                                                       |
| --------------- | ------------------------------------------------------------- |
| `session_start` | config echo incl. pre-declared spec fields                     |
| `book_change`   | best bid/ask moved for a tracked token (wall `recv_ms`, `book_health`) |
| `origin_event`  | every poll (incl. 304s): `changed`, `uncertainty_ms`, stage breakpoints (`stage_ms`), CDN state (`cdn`) |
| `race_opened`   | origin flip detected; quotes/depth/book-health captured at t0   |
| `race`          | first book move after an origin flip; `latency_ms` (monotonic), before/after quotes, `material` flag |
| `forward_walk_sample` | +0…+500 ms offset sweep of the live ask stack (VWAP, cap, fee → `net_if_win`) |
| `forward_walk_verdict` | `EXECUTABLE_EDGE` / `PHANTOM_EDGE` / `NO_EDGE` per spec   |
| `race_timeout`  | no verdict within the window; partial samples retained          |
| `heartbeat`     | quotes, book health + age, pending races, `logger_drops`        |
| `session_end`   | clean shutdown                                                  |

Note: the first poll of each origin is a baseline "change" and opens a race
that usually times out — expected noise.

## Environment findings (2026-11-07)

- **TLS backend is native-tls (SChannel), not rustls.** `ring` fails to
  compile on this machine's MSVC 14.44 (`cl.exe D8050`). See comment in the
  root `Cargo.toml`.
- **`static.crates.io` is intermittently very slow from this machine.**
  `.cargo/config.toml` relaxes cargo's stall detector; plain `cargo fetch`
  works, the default settings do not.
- **`gamma-api.polymarket.com` frequently fails over IPv6 from this machine**
  (curl exits 000); the CLOB REST/WS hosts are fine. Discovery therefore uses
  `clob.polymarket.com/sampling-markets`. `/markets` also works but walks
  from the oldest (closed) markets first.
- CLOB market docs have inconsistent scalar types (`"price": 0` as an
  integer on dead markets); the parser is deliberately lenient.
- `seconds_delay` is present on CLOB `/sampling-markets` docs (verified
  2026-11-07): 996/1000 markets on the first page are `0`, three are `3`
  (live-sports taker buffer), one is `1`. Powers `list --zero-delay`.
- NOAA `hawaii.txt` honours conditional requests: repeat poll with
  `If-None-Match`/`If-Modified-Since` returns `304 Not Modified` (verified
  2026-11-07), so steady-state polling costs headers only.
- USGS `all_day.geojson` sends `Last-Modified` (no ETag) and honours
  `If-Modified-Since` with 304, but sits behind CloudFront with
  `Cache-Control: public, max-age=60` — HIT responses can be up to 60 s
  stale (spec: measurement void in release windows). The FDSN query
  endpoint (`/fdsnws/event/1/query`) was observed returning
  `X-Cache: Miss` (dynamic).
- First live spec-conformant smoke (2026-11-07, earthquake config): 126/129
  polls were 304s, uncertainty window p50 ≈ 1.1 s (≈ poll interval), one
  race resolved at 1,056 ms latency, forward-walk verdicts NO_EDGE, book
  Healthy throughout, 0 logger drops.
- First live target: NOAA Coral Reef Watch `hawaii.txt` (daily append ~21:00Z,
  sends ETag/Last-Modified). Market: "Will Hawaii reach Coral Bleaching Alert
  Level 1 by Dec 31 2026" (DHW ≥ 4). DHW was ~0.43 at setup, so no trigger is
  expected; this target validates the plumbing, not the edge.

## Next steps

1. ~~Verify 304 responses on repeat polls of `hawaii.txt`~~ — done
   2026-11-07 (see Environment findings).
2. ~~Find higher-cadence data-release markets~~ — done 2026-11-07. Full
   scan: 6,689 zero-delay URL-source markets, 413 recurring series
   (`scripts/scan_candidates.py`). Selected target family: **USGS
   daily biggest-earthquake bins** (12 instances, daily cadence, live
   GeoJSON feed, token ids in `configs/earthquake-2026-11-07.json`).
3. ~~Python analysis scripts~~ — done 2026-11-07:
   `scripts/analyze_races.py` (latency histogram, forward-walk EV decay,
   uncertainty/CDN/book-health stats) and `scripts/scan_candidates.py`
   (recurring-series detection over `list` TSV).
4. ~~Upgrade the harness to `docs/MEASUREMENT-SPEC.md`~~ — done
   2026-11-07: forward-walk sampler with VWAP sweeps and verdicts
   (`crates/engine/src/walk.rs`), publication uncertainty window +
   per-poll stage breakpoints, CDN header capture, book-health state
   machine, monotonic clocks end-to-end, non-blocking logger with drop
   counter. 24/24 tests pass; live smoke verified.
5. **Open:** collect 5+ releases of the earthquake series and score them
   against the go/no-go card (`scripts/analyze_races.py`). Known risk:
   `all_day.geojson` is served from CloudFront with `max-age=60` —
   `X-Cache: Hit` responses may be up to 60 s stale, which per the spec
   voids measurements in the release window. Candidate fix: poll the FDSN
   query endpoint instead (observed `X-Cache: Miss` = dynamic).
6. Only if logs show a repeatable window: revisit scope (execution is a
   separate, explicit decision gated by the spec's scorecard — nothing here
   should accrete it silently).