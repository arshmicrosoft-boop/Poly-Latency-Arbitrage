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

# Run the race logger (Ctrl-C to stop; log lands in data/race-<ts>.jsonl)
cargo run -p cashcat -- watch --config configs/coral-hawaii.json
```

### Config format (`watch --config`)

```json
{
  "window_s": 60,        // how long to wait for a book move after origin flip
  "heartbeat_s": 30,
  "data_dir": "data",
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
| `session_start` | config echo                                                    |
| `book_change`   | best bid/ask moved for a tracked token (with `recv_ms`)        |
| `origin_event`  | origin body changed (sha256 differs from previous poll)        |
| `race`          | book moved within `window_s` after an origin flip; has `latency_ms` and before/after quotes |
| `race_timeout`  | no book move within the window; has before quotes              |
| `heartbeat`     | periodic quotes + pending race count                           |
| `session_end`   | clean shutdown                                                 |

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
- First live target: NOAA Coral Reef Watch `hawaii.txt` (daily append ~21:00Z,
  sends ETag/Last-Modified). Market: "Will Hawaii reach Coral Bleaching Alert
  Level 1 by Dec 31 2026" (DHW ≥ 4). DHW was ~0.43 at setup, so no trigger is
  expected; this target validates the plumbing, not the edge.

## Next steps (not built)

1. Verify 304 responses on repeat polls of `hawaii.txt` (baseline run only
   observed the initial 200).
2. Find higher-cadence data-release markets (daily/weekly official stats,
   scheduled macro prints) — daily origins give too few race events to
   measure an edge.
3. Add a Python analysis script over `data/*.jsonl` (latency histograms,
   depth-at-quote before/after).
4. Only if logs show a repeatable window: revisit scope (execution is a
   separate, explicit decision — nothing here should accrete it silently).