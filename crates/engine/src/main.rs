//! cashcat — dry-run latency-arbitrage measurement harness.
//!
//! READ-ONLY BY DESIGN: subscribes to the public Polymarket book feed and
//! polls public origin endpoints; logs JSONL; never holds keys and has no
//! order-placement code path. If the logs never show a tradeable window,
//! that is the answer, and nothing here should grow an execution path.
//!
//! Measurement-spec features (docs/MEASUREMENT-SPEC.md):
//! - monotonic clock for every latency difference; wall clock for logging only
//! - per-poll stage breakpoints + publication uncertainty window + CDN
//!   cache state (captured by `feeds::HttpPoller`, echoed into origin_event)
//! - book health state machine; races/forward-walks only credit a verified book
//! - post-signal forward walk at +0/+25/+50/+100/+250/+500 ms with
//!   multi-level VWAP sweeps and PHANTOM_EDGE / EXECUTABLE_EDGE verdicts
//! - non-blocking logger: bounded channel, drop-and-count, never stalls
//!
//! Pass-3 features:
//! - shared-origin poller dedupe: targets referencing the same origin key
//!   share ONE poller; each poll result is broadcast to all subscribers
//!   (fixes N× request load and the USGS 429s from parallel pollers)
//! - semantic signals: per-target `feeds::Extractor` maps the body to a
//!   monotonic value (daily max magnitude, tweet counter); races open only
//!   when the signal INCREASES, suppressing sub-threshold noise (metadata
//!   hash churn, M1.4 tremors below every bin)
//! - deterministic bin-boundary routing: when a monotonic signal crosses a
//!   bin's upper bound the bin is eliminated (YES→$0, NO locks $1) and the
//!   forward walk runs on the bin's NO asks (`BIN_ELIMINATED`); crossing the
//!   lower bound of the unbounded top bin walks its YES asks
//!   (`TOP_BIN_LOCKED`); entering a bounded bin mid-window is tagged
//!   `PROVISIONAL_SHIFT` (not a deterministic lock). `RAW_HASH` is the
//!   legacy body-hash fallback for unstructured feeds.

mod book;
mod walk;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use book::{BookHealth, BookTracker};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

#[derive(Parser)]
#[command(
    name = "cashcat",
    version,
    about = "Dry-run latency-arbitrage measurement harness (read-only, no orders)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List active CLOB markets, optionally filtered by regex on question+description.
    List {
        /// Max cursor pages to scan (each page is up to ~500 markets).
        #[arg(long, default_value_t = 20)]
        max_pages: u32,
        #[arg(long)]
        regex: Option<String>,
        /// Only markets whose question/description contains a URL (API-resolvable candidates).
        #[arg(long)]
        url_only: bool,
        /// Only markets with seconds_delay == 0 (instant matching). Markets
        /// with a taker delay (e.g. 3 s on live sports) are not snipeable —
        /// see docs/EXECUTION-REFERENCE.md §1.
        #[arg(long)]
        zero_delay: bool,
    },
    /// Run the race logger: live book feed + origin pollers, JSONL output.
    Watch {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Deserialize)]
struct Config {
    /// Named origin endpoints. Targets referencing the same key share ONE
    /// poller (pass 3): N bins on one feed cost one request per interval,
    /// not N — this is what keeps aggregate USGS load under its ~1 req/s
    /// 429 threshold.
    origins: HashMap<String, OriginCfg>,
    targets: Vec<Target>,
    #[serde(default = "default_window")]
    window_s: u64,
    #[serde(default = "default_heartbeat")]
    heartbeat_s: u64,
    #[serde(default = "default_data_dir")]
    data_dir: String,
    /// Forward-walk / sweep parameters (pre-declared, per spec §5/§6).
    /// Defaults follow docs/EXECUTION-REFERENCE.md §5 discipline.
    #[serde(default = "default_notional")]
    target_notional_usd: f64,
    #[serde(default = "default_cap_price")]
    cap_price: f64,
    #[serde(default)]
    taker_fee_bps: f64,
    /// Minimum depth in USDC at the best quote for a race to count as
    /// "material" (spec `min_displayed_depth_usd`).
    #[serde(default = "default_min_depth")]
    min_depth_usd: f64,
    /// Minimum net_if_win at +100/+250 ms to qualify as EXECUTABLE_EDGE.
    #[serde(default = "default_min_profit")]
    min_profit_usd: f64,
    /// A healthy book with no updates for this long is flagged Stale.
    #[serde(default = "default_stale")]
    book_stale_after_s: u64,
}

fn default_window() -> u64 {
    30
}
fn default_heartbeat() -> u64 {
    60
}
fn default_data_dir() -> String {
    "data".into()
}
fn default_notional() -> f64 {
    10.0
}
fn default_cap_price() -> f64 {
    0.95
}
fn default_min_depth() -> f64 {
    25.0
}
fn default_min_profit() -> f64 {
    1.0
}
fn default_stale() -> u64 {
    120
}

#[derive(Deserialize)]
struct OriginCfg {
    /// Poll URL. May contain the `{now_utc}` placeholder, replaced with the
    /// current UTC timestamp (second precision) on every poll. That is the
    /// CloudFront cache-bust for FDSN: repeat polls of an identical URL
    /// return `X-Cache: Hit` bodies up to `max-age=60` stale, while a fresh
    /// `endtime` value forces a Miss (verified by 10-poll probe
    /// 2026-11-07). Extra junk params are rejected 400 by FDSN, so a real
    /// filtering param must carry the variance.
    url: String,
    #[serde(default = "default_interval")]
    interval_ms: u64,
}

#[derive(Deserialize)]
struct Target {
    id: String,
    #[serde(default)]
    note: Option<String>,
    /// Key into `Config::origins`; targets sharing a key share one poller.
    origin: String,
    /// Semantic signal extractor (pass 3). Defaults to RawHash: fire on any
    /// body-hash change (legacy behavior for unstructured feeds).
    #[serde(default)]
    signal: feeds::Extractor,
    /// Bin bounds on the extracted signal. `upper_bound: Some(u)` ⇒ an
    /// eliminable bin: once signal ≥ u the bin's YES is deterministically
    /// $0 and its NO locks to $1 (walk the NO asks). `upper_bound: None`
    /// with `lower_bound: Some(l)` ⇒ the unbounded top bin: signal ≥ l
    /// locks its YES (walk the YES asks). Bounds are half-open `[lower,
    /// upper)` — magnitudes are compared after rounding to 1 decimal, so
    /// the "5.3 to 5.4" bracket is [5.3, 5.5).
    #[serde(default)]
    lower_bound: Option<f64>,
    #[serde(default)]
    upper_bound: Option<f64>,
    yes_token_id: String,
    /// NO-side ERC-1155 token. Required for every bounded bin: bin-
    /// elimination races walk THIS token's asks, not the YES side.
    #[serde(default)]
    no_token_id: Option<String>,
}

fn default_interval() -> u64 {
    1000
}

/// Monotonic → wall-clock conversion anchor. Wall time is used ONLY for
/// log lines; every latency difference uses the monotonic clock (spec §3).
fn wall_of(t: Instant) -> u128 {
    let now_mono = Instant::now();
    let now_wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    now_wall
        .checked_sub(if now_mono > t { now_mono - t } else { Duration::ZERO })
        .unwrap_or_default()
        .as_millis()
}

fn now_wall_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Seconds since Unix epoch → `YYYY-MM-DDTHH:MM:SS` (UTC). Hand-rolled
/// civil-from-days (Howard Hinnant's public-domain algorithm) to avoid a
/// chrono dependency for one format string.
fn utc_stamp(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let tod = epoch_secs % 86_400;
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y0 = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let mo = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if mo <= 2 { y0 + 1 } else { y0 };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}")
}

/// Append-only JSONL logger behind a bounded channel: the hot path only
/// pays a channel send; disk I/O happens on a dedicated task. Drops are
/// counted and reported on heartbeats (spec §9: drop-and-count, never
/// stall the feed handler).
struct Jsonl {
    tx: tokio::sync::mpsc::Sender<String>,
    drops: std::sync::atomic::AtomicU64,
}

impl Jsonl {
    fn create(path: &std::path::Path, buffer: usize) -> Result<Self> {
        let f = std::fs::File::create(path)
            .with_context(|| format!("create {}", path.display()))?;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(buffer.max(64));
        tokio::spawn(async move {
            use std::io::Write as _;
            let mut w = std::io::BufWriter::with_capacity(64 * 1024, f);
            while let Some(line) = rx.recv().await {
                if writeln!(w, "{line}").is_err() {
                    break;
                }
                let _ = w.flush(); // flush policy: one line out per event
            }
        });
        Ok(Self {
            tx,
            drops: std::sync::atomic::AtomicU64::new(0),
        })
    }

    fn emit(&self, mut v: Value) {
        if let Some(o) = v.as_object_mut() {
            o.insert("ts_ms".into(), json!(now_wall_ms()));
        }
        if self.tx.try_send(v.to_string()).is_err() {
            self.drops
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn drop_count(&self) -> u64 {
        self.drops.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Close the channel and wait for the writer to drain.
    async fn close(self) {
        let tx = self.tx;
        drop(tx);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// First http(s) URL in the text, stripped of surrounding punctuation —
/// the candidate resolution origin named in the market question/description.
fn first_url(hay: &str) -> Option<String> {
    for tok in hay.split_whitespace() {
        let t = tok.trim_matches(|c: char| "()[]{}\"'<>,;".contains(c));
        if t.starts_with("http://") || t.starts_with("https://") {
            return Some(t.trim_end_matches(['.', ',', ')', ']', ';', ':']).to_string());
        }
    }
    None
}

/// How a signal increase routes onto a bin (pass 3). See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalClass {
    /// Signal crossed this bounded bin's upper bound ⇒ YES is $0, NO locks
    /// to $1 deterministically. Walk the NO asks.
    BinEliminated,
    /// Signal crossed the unbounded top bin's lower bound ⇒ its YES locks.
    /// Walk the YES asks.
    TopBinLocked,
    /// Signal entered a bounded bin without exiting it: the bin got more
    /// likely but nothing is locked yet. Walk YES asks, tagged provisional —
    /// the analyzer reports these separately from deterministic walks.
    ProvisionalShift,
    /// Legacy body-hash race (unstructured feed). Walk YES asks.
    RawHash,
}

impl SignalClass {
    fn as_str(self) -> &'static str {
        match self {
            SignalClass::BinEliminated => "BIN_ELIMINATED",
            SignalClass::TopBinLocked => "TOP_BIN_LOCKED",
            SignalClass::ProvisionalShift => "PROVISIONAL_SHIFT",
            SignalClass::RawHash => "RAW_HASH",
        }
    }
}

/// Deterministic bin-boundary routing for one signal increase `old → new`.
/// Bounds are half-open `[lower, upper)`; `new` is the extractor's rounded
/// value, so `new >= upper` means the bin cannot win. A jump that crosses
/// both bounds resolves to elimination (the stronger, deterministic claim).
/// Returns `None` when no boundary relevant to THIS bin was crossed (the
/// signal moved inside the bin, or the bin was already settled).
fn route_crossing(
    lower: Option<f64>,
    upper: Option<f64>,
    old: f64,
    new: f64,
) -> Option<SignalClass> {
    match upper {
        Some(u) => {
            if old < u && new >= u {
                return Some(SignalClass::BinEliminated);
            }
            if let Some(l) = lower {
                if old < l && new >= l && new < u {
                    return Some(SignalClass::ProvisionalShift);
                }
            }
            None
        }
        None => {
            let l = lower?;
            if old < l && new >= l {
                Some(SignalClass::TopBinLocked)
            } else {
                None
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cashcat=info,clob=info".into()),
        )
        .init();
    match Cli::parse().cmd {
        Cmd::List { max_pages, regex, url_only, zero_delay } => {
            list(max_pages, regex, url_only, zero_delay).await
        }
        Cmd::Watch { config } => watch(config).await,
    }
}

async fn list(max_pages: u32, regex: Option<String>, url_only: bool, zero_delay: bool) -> Result<()> {
    let client = clob::client()?;
    let re = regex
        .map(|r| regex::Regex::new(&r))
        .transpose()
        .context("compile --regex")?;
    let mut cursor = String::new();
    let mut shown = 0usize;
    for page in 0..max_pages {
        // /sampling-markets: currently active markets (/markets starts from
        // the oldest, closed ones).
        let p = clob::list_sampling_page(&client, &cursor).await?;
        for m in &p.data {
            if !m.active || m.closed || !m.accepting_orders {
                continue;
            }
            let hay = format!("{} {}", m.question, m.description);
            if let Some(re) = &re {
                if !re.is_match(&hay) {
                    continue;
                }
            }
            let has_url = hay.contains("http://") || hay.contains("https://");
            if url_only && !has_url {
                continue;
            }
            let delay = m.seconds_delay.clone().unwrap_or_else(|| "?".into());
            if zero_delay && delay != "0" {
                continue;
            }
            let mut tags = Vec::new();
            if has_url {
                tags.push("url-src");
            }
            if delay != "0" {
                tags.push(match delay.as_str() {
                    "3" => "delay-3s",
                    "?" => "delay-?",
                    _ => "delay",
                });
            }
            println!(
                "{}\t{}\t{}\t{}\t{}",
                m.market_slug,
                m.end_date_iso.clone().unwrap_or_default(),
                m.question,
                tags.join(","),
                first_url(&hay).unwrap_or_default()
            );
            shown += 1;
        }
        info!(page = page + 1, scanned_so_far = shown, "scanned page");
        if p.next_cursor.is_empty() || p.next_cursor == "LTE=" {
            break;
        }
        cursor = p.next_cursor;
    }
    eprintln!("# {shown} markets shown");
    Ok(())
}

struct Pending {
    origin_id: u64,
    target_idx: usize,
    /// Token whose asks are walked and whose book movement resolves the
    /// race — the side a trade would actually hit: the NO token for
    /// BIN_ELIMINATED, the YES token otherwise.
    walk_token_id: String,
    signal_class: &'static str,
    old_signal: Option<f64>,
    new_signal: Option<f64>,
    t0: Instant,
    t0_wall_ms: u128,
    uncertainty_ms: Option<u128>,
    ask_before: Option<f64>,
    bid_before: Option<f64>,
    book_health_at_open: &'static str,
    // Forward-walk state.
    walk_offsets_done: u32,
    samples: Vec<(u128, f64)>, // (offset_ms, net_if_win)
    book_healthy_all: bool,
    resolved: bool,
}

const WALK_OFFSETS_MS: [u64; 6] = [0, 25, 50, 100, 250, 500];

struct OriginMsg {
    origin_idx: usize,
    /// Shared across all subscribers of this origin: one poll, one body,
    /// N extractors.
    ev: Arc<feeds::OriginEvent>,
}

async fn watch(path: PathBuf) -> Result<()> {
    let cfg: Config = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .with_context(|| format!("read config {}", path.display()))?,
    )
    .context("parse config")?;
    if cfg.targets.is_empty() {
        anyhow::bail!("config has no targets");
    }
    for t in &cfg.targets {
        if !cfg.origins.contains_key(&t.origin) {
            anyhow::bail!("target '{}' references unknown origin key '{}'", t.id, t.origin);
        }
        if t.upper_bound.is_some() && t.no_token_id.is_none() {
            anyhow::bail!(
                "target '{}' is a bounded bin but has no no_token_id — bin-elimination walks need the NO side",
                t.id
            );
        }
    }
    std::fs::create_dir_all(&cfg.data_dir)?;
    let log_path = std::path::Path::new(&cfg.data_dir).join(format!("race-{}.jsonl", now_wall_ms()));
    let log = std::sync::Arc::new(Jsonl::create(&log_path, 8192)?);
    info!(log = %log_path.display(), targets = cfg.targets.len(), "session start");

    let client = clob::client()?;
    // Deduped token universe: every YES token plus every NO token — bin-
    // elimination walks NO asks, so both sides need book coverage.
    let mut token_ids: Vec<String> = Vec::new();
    for t in &cfg.targets {
        for tok in std::iter::once(&t.yes_token_id).chain(t.no_token_id.as_ref()) {
            if !token_ids.contains(tok) {
                token_ids.push(tok.clone());
            }
        }
    }

    // Initialize book trackers from REST snapshots.
    let mut books: HashMap<String, BookTracker> = HashMap::new();
    for t in &token_ids {
        match clob::get_book(&client, t).await {
            Ok(b) => {
                books.insert(t.clone(), BookTracker::from_rest(&b));
            }
            Err(e) => warn!(token = %t, error = %e, "initial book sync failed; waiting for WS snapshot"),
        }
    }

    // WS feed task with reconnect/backoff. On disconnect/reconnect we mark
    // the affected books so signals are never generated from an unverified
    // book (spec §4).
    let (ws_tx, mut ws_rx) = tokio::sync::mpsc::channel::<clob::ws::FeedMsg>(4096);
    let feed_tokens = token_ids.clone();
    tokio::spawn(async move {
        let mut backoff = 1u64;
        loop {
            match clob::ws::run_market_feed(feed_tokens.clone(), ws_tx.clone()).await {
                Ok(()) => {
                    warn!("ws feed closed");
                    backoff = 1;
                }
                Err(e) => warn!(error = %e, "ws feed error"),
            }
            // Signal feed loss: receivers treat a channel close as
            // Disconnected for all tracked books.
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    });

    // Shared-origin poller dedupe (pass 3, Task 1): one poller per origin
    // KEY, in first-reference order; each poll result is broadcast to every
    // subscribed target. N bins on one feed ⇒ one request per interval.
    let mut origin_order: Vec<String> = Vec::new();
    let mut origin_targets: Vec<Vec<usize>> = Vec::new();
    for (ti, t) in cfg.targets.iter().enumerate() {
        match origin_order.iter().position(|k| k == &t.origin) {
            Some(oi) => origin_targets[oi].push(ti),
            None => {
                origin_order.push(t.origin.clone());
                origin_targets.push(vec![ti]);
            }
        }
    }
    info!(
        origins = origin_order.len(),
        targets = cfg.targets.len(),
        "pollers spawned (deduped by origin key)"
    );
    let (origin_tx, mut origin_rx) = tokio::sync::mpsc::channel::<OriginMsg>(256);
    for (oi, key) in origin_order.iter().enumerate() {
        let o = &cfg.origins[key];
        let url_template = o.url.clone();
        let mut poller = feeds::HttpPoller::new(
            client.clone(),
            o.url.clone(),
            Duration::from_millis(o.interval_ms),
        );
        let tx = origin_tx.clone();
        let interval = poller.interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                let scheduled = ticker.tick().await;
                // tokio::time::Instant -> std::time::Instant (feeds crate
                // uses std monotonic throughout, spec §3).
                let res = if url_template.contains("{now_utc}") {
                    let url = url_template.replace("{now_utc}", &utc_stamp(now_epoch_secs()));
                    poller.poll_url(scheduled.into(), &url).await
                } else {
                    poller.poll_once(scheduled.into()).await
                };
                match res {
                    Ok(ev) => {
                        if tx.send(OriginMsg { origin_idx: oi, ev: Arc::new(ev) }).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => warn!(origin = oi, error = %e, "origin poll failed"),
                }
            }
        });
    }

    log.emit(json!({
        "kind": "session_start",
        "window_s": cfg.window_s,
        "heartbeat_s": cfg.heartbeat_s,
        "target_notional_usd": cfg.target_notional_usd,
        "cap_price": cfg.cap_price,
        "taker_fee_bps": cfg.taker_fee_bps,
        "min_depth_usd": cfg.min_depth_usd,
        "min_profit_usd": cfg.min_profit_usd,
        "book_stale_after_s": cfg.book_stale_after_s,
        "origins": origin_order.iter().enumerate().map(|(oi, k)| json!({
            "key": k,
            "url": cfg.origins[k].url,
            "interval_ms": cfg.origins[k].interval_ms,
            "subscribers": origin_targets[oi].len()
        })).collect::<Vec<_>>(),
        "targets": cfg.targets.iter().map(|t| json!({
            "id": t.id, "origin": t.origin,
            "signal": format!("{:?}", t.signal),
            "lower_bound": t.lower_bound, "upper_bound": t.upper_bound,
            "yes_token_id": t.yes_token_id, "no_token_id": t.no_token_id,
            "note": t.note
        })).collect::<Vec<_>>(),
    }));

    let mut pending: Vec<Pending> = Vec::new();
    let mut origin_seq: u64 = 0;
    // Last extracted signal per target; `None` until the baseline poll.
    let mut last_signal: Vec<Option<f64>> = vec![None; cfg.targets.len()];
    let stale_after = Duration::from_secs(cfg.book_stale_after_s.max(10));
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    // Forward-walk offsets need sub-tick resolution: the spec's +0/+25/+50 ms
    // samples would otherwise land ~1 s late on a 1 s tick (observed in the
    // first smoke run: all six samples clumped at +1119 ms). 20 ms keeps
    // offset jitter inside half a bucket.
    let mut walk_tick = tokio::time::interval(Duration::from_millis(20));
    walk_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(cfg.heartbeat_s.max(5)));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Heartbeat covers both sides of every bin (walks may hit either).
    let hb_tokens: Vec<(String, String)> = cfg
        .targets
        .iter()
        .flat_map(|t| {
            let mut v = vec![(format!("{}#YES", t.id), t.yes_token_id.clone())];
            if let Some(n) = &t.no_token_id {
                v.push((format!("{}#NO", t.id), n.clone()));
            }
            v
        })
        .collect();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c, flushing and exiting");
                break;
            }
            msg = ws_rx.recv() => {
                let Some(clob::ws::FeedMsg { recv_at, event }) = msg else {
                    // Feed task gone: no more book data this session.
                    for b in books.values_mut() {
                        b.mark_disconnected();
                    }
                    break;
                };
                match event {
                    clob::ws::WsEvent::Book { asset_id, bids, asks } => {
                        let tracker = books.entry(asset_id.clone()).or_default();
                        tracker.load_snapshot(&bids, &asks);
                        on_quotes(&log, &cfg, &mut pending, &asset_id, tracker, recv_at);
                    }
                    clob::ws::WsEvent::PriceChange { asset_id, changes } => {
                        if let Some(tracker) = books.get_mut(&asset_id) {
                            for c in changes {
                                tracker.apply_change(c.side, c.price, c.size);
                            }
                            on_quotes(&log, &cfg, &mut pending, &asset_id, tracker, recv_at);
                        }
                    }
                    clob::ws::WsEvent::LastTrade { .. } => {}
                    clob::ws::WsEvent::Other(_) => {}
                }
            }
            msg = origin_rx.recv() => {
                let Some(OriginMsg { origin_idx, ev }) = msg else { break };
                let subscribers = &origin_targets[origin_idx];
                // One origin_event per POLL (shared poller), not per target:
                // release-completeness needs the full poll record (spec §9).
                log.emit(json!({
                    "kind": "origin_event",
                    "origin_key": origin_order[origin_idx],
                    "subscribers": subscribers.len(),
                    "status": ev.status,
                    "changed": ev.changed,
                    "body_len": ev.body_len,
                    "body_sha256": ev.body_sha256,
                    "etag": ev.etag,
                    "uncertainty_ms": ev.uncertainty_ms,
                    "stage_ms": {
                        "sched_to_req": ev.t_req_sent.duration_since(ev.t_scheduled).as_millis(),
                        "req_to_headers": ev.t_headers_rx.duration_since(ev.t_req_sent).as_millis(),
                        "headers_to_body": ev.t_body_rx.duration_since(ev.t_headers_rx).as_millis(),
                        "body_to_parse": ev.t_parse_done.duration_since(ev.t_body_rx).as_millis(),
                    },
                    "cdn": {
                        "cf_cache_status": ev.cf_cache_status,
                        "x_cache": ev.x_cache,
                        "age": ev.age,
                        "date": ev.http_date,
                    },
                }));
                if ev.status != 200 {
                    continue; // 429/5xx: no signal this tick; logged above
                }
                for &ti in subscribers {
                    let target = &cfg.targets[ti];
                    match target.signal.extract(&ev.body) {
                        None => {
                            if matches!(target.signal, feeds::Extractor::RawHash) {
                                // Legacy path: fire on any body-hash change.
                                if !ev.changed {
                                    continue;
                                }
                                open_race(
                                    &log, &cfg, &mut pending, &books, ti,
                                    target.yes_token_id.clone(), SignalClass::RawHash,
                                    None, None, &ev, &mut origin_seq,
                                );
                            } else {
                                // Garbage body: never race on a feed we
                                // cannot read.
                                log.emit(json!({
                                    "kind": "signal_parse_failed",
                                    "target_id": target.id,
                                    "status": ev.status,
                                    "body_len": ev.body_len,
                                }));
                            }
                        }
                        Some(v) => {
                            let prev = last_signal[ti];
                            match prev {
                                None => {
                                    // First poll establishes the baseline;
                                    // racing the initial state is noise.
                                    last_signal[ti] = Some(v);
                                    log.emit(json!({
                                        "kind": "signal_baseline",
                                        "target_id": target.id,
                                        "value": v,
                                    }));
                                    continue;
                                }
                                Some(p) if v <= p => {
                                    // Monotonic signals: no increase ⇒ no
                                    // race. Suppresses metadata-hash churn
                                    // and sub-threshold events that move no
                                    // bin (the pass-3 noise fix).
                                    continue;
                                }
                                Some(_) => {}
                            }
                            let old = prev.unwrap();
                            match route_crossing(target.lower_bound, target.upper_bound, old, v) {
                                None => {
                                    // Signal moved inside the bin (e.g. a
                                    // tweet count tick): no boundary crossed,
                                    // nothing deterministic to trade.
                                    last_signal[ti] = Some(v);
                                    log.emit(json!({
                                        "kind": "signal_update",
                                        "target_id": target.id,
                                        "old": old, "new": v, "routed": null,
                                    }));
                                }
                                Some(cls) => {
                                    let (walk_token, class) = match cls {
                                        SignalClass::BinEliminated => {
                                            let Some(no_tok) = &target.no_token_id else {
                                                // Checked at config load;
                                                // keep a guard anyway.
                                                last_signal[ti] = Some(v);
                                                warn!(target = %target.id, "BIN_ELIMINATED without no_token_id; skipped");
                                                continue;
                                            };
                                            (no_tok.clone(), SignalClass::BinEliminated)
                                        }
                                        SignalClass::TopBinLocked => (target.yes_token_id.clone(), cls),
                                        SignalClass::ProvisionalShift => (target.yes_token_id.clone(), cls),
                                        SignalClass::RawHash => {
                                            unreachable!("route_crossing never returns RawHash")
                                        }
                                    };
                                    last_signal[ti] = Some(v);
                                    log.emit(json!({
                                        "kind": "signal_update",
                                        "target_id": target.id,
                                        "old": old, "new": v,
                                        "routed": class.as_str(),
                                    }));
                                    open_race(
                                        &log, &cfg, &mut pending, &books, ti,
                                        walk_token, class, Some(old), Some(v),
                                        &ev, &mut origin_seq,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            _ = walk_tick.tick() => {
                advance_walks(&log, &cfg, &mut pending, &mut books);
            }
            _ = tick.tick() => {
                // Time out races that produced no verdict within the window.
                let now = Instant::now();
                let window = Duration::from_secs(cfg.window_s);
                let mut i = 0;
                while i < pending.len() {
                    if now - pending[i].t0 >= window {
                        let p = pending.remove(i);
                        log.emit(json!({
                            "kind": "race_timeout",
                            "origin_id": p.origin_id,
                            "target_id": cfg.targets[p.target_idx].id,
                            "signal_class": p.signal_class,
                            "walk_token_id": p.walk_token_id,
                            "window_ms": cfg.window_s * 1000,
                            "ask_before": p.ask_before,
                            "bid_before": p.bid_before,
                            "samples": p.samples.iter().map(|(o, e)| json!({"offset_ms": o, "net_if_win": e})).collect::<Vec<_>>(),
                        }));
                    } else {
                        i += 1;
                    }
                }

                // Stale-feed detection.
                for b in books.values_mut() {
                    b.stale_if_quiet(stale_after);
                }
            }
            _ = heartbeat.tick() => {
                let quotes: Vec<Value> = hb_tokens.iter().map(|(label, tok)| {
                    let book = books.get(tok);
                    let (b, a) = book
                        .map(|x| (x.best_bid().map(|q| q.0), x.best_ask().map(|q| q.0)))
                        .unwrap_or((None, None));
                    let health = book.map_or("Missing", |x| x.health().as_str());
                    let age_ms = book.and_then(|x| x.last_update()).map(|u| u.elapsed().as_millis());
                    json!({"token_label": label, "best_bid": b, "best_ask": a, "book_health": health, "book_age_ms": age_ms})
                }).collect();
                log.emit(json!({
                    "kind": "heartbeat",
                    "quotes": quotes,
                    "pending_races": pending.len(),
                    "logger_drops": log.drop_count(),
                }));
            }
        }
    }
    log.emit(json!({"kind": "session_end", "logger_drops": log.drop_count()}));
    let log_arc = log;
    let inner = std::sync::Arc::try_unwrap(log_arc).ok();
    if let Some(l) = inner {
        l.close().await;
    }
    Ok(())
}

fn sweep_fields(s: &walk::Sweep) -> Value {
    json!({
        "spent": s.spent,
        "shares": s.shares,
        "avg_price": s.avg_price,
        "fully_filled": s.fully_filled,
        "cap_limited": s.cap_limited,
        "fee": s.fee,
        "net_if_win": s.net_if_win,
    })
}

/// Snapshot the walk token's book and push a Pending race. `walk_token` is
/// the side a trade would actually hit: NO asks for BIN_ELIMINATED, YES
/// asks for TOP_BIN_LOCKED / PROVISIONAL_SHIFT / RAW_HASH.
fn open_race(
    log: &Jsonl,
    cfg: &Config,
    pending: &mut Vec<Pending>,
    books: &HashMap<String, BookTracker>,
    target_idx: usize,
    walk_token: String,
    class: SignalClass,
    old_signal: Option<f64>,
    new_signal: Option<f64>,
    ev: &feeds::OriginEvent,
    origin_seq: &mut u64,
) {
    *origin_seq += 1;
    let target = &cfg.targets[target_idx];
    let book = books.get(&walk_token);
    let (bid, ask) = book
        .map(|b| (b.best_bid().map(|q| q.0), b.best_ask().map(|q| q.0)))
        .unwrap_or((None, None));
    let depth_ok = book.map_or(false, |b| {
        b.best_ask().map_or(false, |(p, s)| p * s >= cfg.min_depth_usd)
    });
    pending.push(Pending {
        origin_id: *origin_seq,
        target_idx,
        walk_token_id: walk_token.clone(),
        signal_class: class.as_str(),
        old_signal,
        new_signal,
        t0: ev.t_headers_rx,
        t0_wall_ms: ev.at.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
        uncertainty_ms: ev.uncertainty_ms,
        ask_before: ask,
        bid_before: bid,
        book_health_at_open: book.map_or(BookHealth::Uninitialized, |b| b.health()).as_str(),
        walk_offsets_done: 0,
        samples: Vec::new(),
        book_healthy_all: true,
        resolved: false,
    });
    log.emit(json!({
        "kind": "race_opened",
        "origin_id": *origin_seq,
        "target_id": target.id,
        "signal_class": class.as_str(),
        "walk_token_id": walk_token,
        "old_signal": old_signal,
        "new_signal": new_signal,
        "t0_wall_ms": pending.last().unwrap().t0_wall_ms,
        "ask_before": ask,
        "bid_before": bid,
        "depth_ok_at_open": depth_ok,
        "book_health": pending.last().unwrap().book_health_at_open,
        "uncertainty_ms": ev.uncertainty_ms,
    }));
}

/// Advance forward-walk sampling for all pending races at the fixed spec
/// offsets (+0/+25/+50/+100/+250/+500 ms) and emit a verdict once all six
/// samples are in. Runs on the 20 ms walk tick so sample offsets actually
/// land near their targets instead of clumping on the 1 s tick.
fn advance_walks(
    log: &Jsonl,
    cfg: &Config,
    pending: &mut Vec<Pending>,
    books: &mut HashMap<String, BookTracker>,
) {
    if pending.is_empty() {
        return;
    }
    let now = Instant::now();
    let mut i = 0;
    while i < pending.len() {
        let elapsed = now - pending[i].t0;
        let walk_token = pending[i].walk_token_id.clone();

        while pending[i].walk_offsets_done < WALK_OFFSETS_MS.len() as u32 {
            let off = WALK_OFFSETS_MS[pending[i].walk_offsets_done as usize];
            if elapsed >= Duration::from_millis(off) {
                let sweep = books.get(&walk_token).map_or_else(
                    || walk::sweep_asks(&[], cfg.target_notional_usd, cfg.cap_price, cfg.taker_fee_bps),
                    |b| {
                        if !matches!(b.health(), BookHealth::Healthy | BookHealth::Stale) {
                            pending[i].book_healthy_all = false;
                        }
                        let asks = b.asks_snapshot();
                        walk::sweep_asks(&asks, cfg.target_notional_usd, cfg.cap_price, cfg.taker_fee_bps)
                    },
                );
                let offset_ms = elapsed.as_millis();
                pending[i].samples.push((offset_ms, sweep.net_if_win));
                log.emit(json!({
                    "kind": "forward_walk_sample",
                    "origin_id": pending[i].origin_id,
                    "target_id": cfg.targets[pending[i].target_idx].id,
                    "signal_class": pending[i].signal_class,
                    "walk_token_id": walk_token,
                    "offset_ms": offset_ms,
                    "sweep": sweep_fields(&sweep),
                    "book_health": books.get(&walk_token).map_or("Missing", |b| b.health().as_str()),
                }));
                pending[i].walk_offsets_done += 1;
            } else {
                break;
            }
        }

        if pending[i].walk_offsets_done >= WALK_OFFSETS_MS.len() as u32 {
            let p = pending.remove(i);
            let verdict = walk::judge(&p.samples, cfg.min_profit_usd, p.book_healthy_all);
            log.emit(json!({
                "kind": "forward_walk_verdict",
                "origin_id": p.origin_id,
                "target_id": cfg.targets[p.target_idx].id,
                "signal_class": p.signal_class,
                "walk_token_id": p.walk_token_id,
                "old_signal": p.old_signal,
                "new_signal": p.new_signal,
                "verdict": verdict.as_str(),
                "book_healthy_all": p.book_healthy_all,
                "min_profit_usd": cfg.min_profit_usd,
            }));
        } else {
            i += 1;
        }
    }
}

/// Called after every applied book update: logs best-quote moves and resolves
/// pending races (origin flip happened; did/when did the book move?). Races
/// resolve on the WALK token — for BIN_ELIMINATED that is the NO side, the
/// one a trade would actually hit.
fn on_quotes(
    log: &Jsonl,
    cfg: &Config,
    pending: &mut Vec<Pending>,
    asset_id: &str,
    tracker: &BookTracker,
    recv_at: Instant,
) {
    let bid = tracker.best_bid().map(|q| q.0);
    let ask = tracker.best_ask().map(|q| q.0);
    log.emit(json!({
        "kind": "book_change",
        "token_id": asset_id,
        "best_bid": bid,
        "best_ask": ask,
        "book_health": tracker.health().as_str(),
        "recv_ms": wall_of(recv_at),
    }));
    // Resolve any pending race walking this token (first material move).
    // Forward-walk sampling continues regardless — the verdict comes from
    // the fixed-offset samples, not the first move.
    let mut i = 0;
    while i < pending.len() {
        let matches_token = !pending[i].resolved && pending[i].walk_token_id == asset_id;
        if matches_token {
            let latency = (recv_at - pending[i].t0).as_millis();
            let material = tracker.best_ask().map_or(false, |(p, s)| {
                let moved = pending[i].ask_before.map_or(true, |a| (p - a).abs() > 1e-9);
                moved && p * s >= cfg.min_depth_usd
            });
            log.emit(json!({
                "kind": "race",
                "origin_id": pending[i].origin_id,
                "target_id": cfg.targets[pending[i].target_idx].id,
                "signal_class": pending[i].signal_class,
                "token_id": asset_id,
                "latency_ms": latency,
                "material": material,
                "uncertainty_ms": pending[i].uncertainty_ms,
                "ask_before": pending[i].ask_before,
                "ask_after": ask,
                "bid_before": pending[i].bid_before,
                "bid_after": bid,
            }));
            pending[i].resolved = true;
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_bin_eliminated() {
        // "5.3 to 5.4" bin = [5.3, 5.5): max crossing 5.5 eliminates it.
        assert_eq!(
            route_crossing(Some(5.3), Some(5.5), 5.4, 5.6),
            Some(SignalClass::BinEliminated)
        );
        // A jump across several bins at once still eliminates this bin.
        assert_eq!(
            route_crossing(Some(5.3), Some(5.5), 4.6, 6.1),
            Some(SignalClass::BinEliminated)
        );
        // Under-bin (no lower bound): crossing its upper bound eliminates.
        assert_eq!(
            route_crossing(None, Some(5.3), 4.6, 5.3),
            Some(SignalClass::BinEliminated)
        );
    }

    #[test]
    fn route_no_refire_after_settled() {
        // Already past the upper bound: a further increase crosses nothing
        // for THIS bin (its elimination race fired on the crossing poll).
        assert_eq!(route_crossing(Some(5.3), Some(5.5), 5.7, 6.2), None);
        // Top bin already locked.
        assert_eq!(route_crossing(Some(6.1), None, 6.3, 6.5), None);
    }

    #[test]
    fn route_top_bin_locked() {
        assert_eq!(
            route_crossing(Some(6.1), None, 5.9, 6.3),
            Some(SignalClass::TopBinLocked)
        );
        // Exact bound value locks (half-open [lower, ∞)).
        assert_eq!(
            route_crossing(Some(6.1), None, 6.0, 6.1),
            Some(SignalClass::TopBinLocked)
        );
    }

    #[test]
    fn route_provisional_shift() {
        // Entering a bounded bin without exiting it: more likely, not locked.
        assert_eq!(
            route_crossing(Some(5.5), Some(5.7), 5.4, 5.6),
            Some(SignalClass::ProvisionalShift)
        );
        // Crossing the upper bound in the same jump is the deterministic
        // elimination, not a provisional entry.
        assert_eq!(
            route_crossing(Some(5.5), Some(5.7), 5.4, 5.8),
            Some(SignalClass::BinEliminated)
        );
        // Under-bins (no lower bound) are the default state: entering them
        // is never a provisional shift.
        assert_eq!(route_crossing(None, Some(5.3), 0.0, 4.9), None);
    }

    #[test]
    fn route_no_boundary_crossed() {
        // Counter ticks within the bin: nothing deterministic to trade.
        assert_eq!(route_crossing(Some(20.0), Some(40.0), 21.0, 22.0), None);
        // Already eliminated, counter keeps climbing.
        assert_eq!(route_crossing(Some(20.0), Some(40.0), 41.0, 42.0), None);
    }

    #[test]
    fn utc_stamp_formats() {
        assert_eq!(utc_stamp(0), "1970-01-01T00:00:00");
        // 20764 days after epoch = 2026-11-07 (leap-day arithmetic check).
        assert_eq!(utc_stamp(1_794_009_600), "2026-11-07T00:00:00");
        assert_eq!(utc_stamp(1_794_009_600 + 86_399), "2026-11-07T23:59:59");
    }
}