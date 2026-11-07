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

mod book;
mod walk;

use std::collections::HashMap;
use std::path::PathBuf;
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
struct Target {
    id: String,
    token_id: String,
    #[serde(default)]
    note: Option<String>,
    origin: OriginCfg,
}

#[derive(Deserialize)]
struct OriginCfg {
    url: String,
    #[serde(default = "default_interval")]
    interval_ms: u64,
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
    target_idx: usize,
    ev: feeds::OriginEvent,
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
    std::fs::create_dir_all(&cfg.data_dir)?;
    let log_path = std::path::Path::new(&cfg.data_dir).join(format!("race-{}.jsonl", now_wall_ms()));
    let log = std::sync::Arc::new(Jsonl::create(&log_path, 8192)?);
    info!(log = %log_path.display(), targets = cfg.targets.len(), "session start");

    let client = clob::client()?;
    let token_ids: Vec<String> = cfg.targets.iter().map(|t| t.token_id.clone()).collect();

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

    // One poller task per target.
    let (origin_tx, mut origin_rx) = tokio::sync::mpsc::channel::<OriginMsg>(256);
    for (i, t) in cfg.targets.iter().enumerate() {
        let mut poller = feeds::HttpPoller::new(
            client.clone(),
            t.origin.url.clone(),
            Duration::from_millis(t.origin.interval_ms),
        );
        let tx = origin_tx.clone();
        let interval = poller.interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                let scheduled = ticker.tick().await;
                // tokio::time::Instant -> std::time::Instant (feeds crate
                // uses std monotonic throughout, spec §3).
                match poller.poll_once(scheduled.into()).await {
                    Ok(ev) => {
                        if tx.send(OriginMsg { target_idx: i, ev }).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => warn!(target = i, error = %e, "origin poll failed"),
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
        "targets": cfg.targets.iter().map(|t| json!({
            "id": t.id, "token_id": t.token_id, "origin": t.origin.url,
            "interval_ms": t.origin.interval_ms, "note": t.note
        })).collect::<Vec<_>>(),
    }));

    let mut pending: Vec<Pending> = Vec::new();
    let mut origin_seq: u64 = 0;
    let stale_after = Duration::from_secs(cfg.book_stale_after_s.max(10));
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(cfg.heartbeat_s.max(5)));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut last_quotes: HashMap<String, (Option<f64>, Option<f64>)> = HashMap::new();

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
                        on_quotes(&log, &cfg, &mut pending, &mut last_quotes, &asset_id, tracker, recv_at);
                    }
                    clob::ws::WsEvent::PriceChange { asset_id, changes } => {
                        if let Some(tracker) = books.get_mut(&asset_id) {
                            for c in changes {
                                tracker.apply_change(c.side, c.price, c.size);
                            }
                            on_quotes(&log, &cfg, &mut pending, &mut last_quotes, &asset_id, tracker, recv_at);
                        }
                    }
                    clob::ws::WsEvent::LastTrade { .. } => {}
                    clob::ws::WsEvent::Other(_) => {}
                }
            }
            msg = origin_rx.recv() => {
                let Some(OriginMsg { target_idx, ev }) = msg else { break };
                let target = &cfg.targets[target_idx];
                // Always log origin events (incl. 304s): release-completeness
                // needs the full poll record, not just changes (spec §9).
                log.emit(json!({
                    "kind": "origin_event",
                    "target_id": target.id,
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
                if !ev.changed {
                    continue; // 304 / identical body: nothing to race on
                }
                origin_seq += 1;
                let book = books.get(&target.token_id);
                let (bid, ask) = book
                    .map(|b| (b.best_bid().map(|q| q.0), b.best_ask().map(|q| q.0)))
                    .unwrap_or((None, None));
                let depth_ok = book.map_or(false, |b| {
                    b.best_ask().map_or(false, |(p, s)| p * s >= cfg.min_depth_usd)
                });
                pending.push(Pending {
                    origin_id: origin_seq,
                    target_idx,
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
                    "origin_id": origin_seq,
                    "target_id": target.id,
                    "ask_before": ask,
                    "bid_before": bid,
                    "depth_ok_at_open": depth_ok,
                    "book_health": pending.last().unwrap().book_health_at_open,
                    "uncertainty_ms": ev.uncertainty_ms,
                }));
            }
            _ = tick.tick() => {
                // Advance forward-walk samples and time out stale races.
                let now = Instant::now();
                let window = Duration::from_secs(cfg.window_s);
                let mut i = 0;
                while i < pending.len() {
                    let elapsed = now - pending[i].t0;
                    let target_idx = pending[i].target_idx;
                    let token_id = cfg.targets[target_idx].token_id.clone();

                    // Sample the book at each fixed offset while offsets remain.
                    while pending[i].walk_offsets_done < WALK_OFFSETS_MS.len() as u32 {
                        let off = WALK_OFFSETS_MS[pending[i].walk_offsets_done as usize];
                        if elapsed >= Duration::from_millis(off) {
                            let sweep = books.get(&token_id).map_or_else(
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
                                "target_id": cfg.targets[target_idx].id,
                                "offset_ms": offset_ms,
                                "sweep": sweep_fields(&sweep),
                                "book_health": books.get(&token_id).map_or("Missing", |b| b.health().as_str()),
                            }));
                            pending[i].walk_offsets_done += 1;
                        } else {
                            break;
                        }
                    }

                    // All offsets done → verdict; then drop the race.
                    if pending[i].walk_offsets_done >= WALK_OFFSETS_MS.len() as u32 {
                        let p = pending.remove(i);
                        let verdict = walk::judge(&p.samples, cfg.min_profit_usd, p.book_healthy_all);
                        log.emit(json!({
                            "kind": "forward_walk_verdict",
                            "origin_id": p.origin_id,
                            "target_id": cfg.targets[p.target_idx].id,
                            "verdict": verdict.as_str(),
                            "book_healthy_all": p.book_healthy_all,
                            "min_profit_usd": cfg.min_profit_usd,
                        }));
                        continue;
                    }

                    // Window timeout: no verdict possible past the window.
                    if elapsed >= window {
                        let p = pending.remove(i);
                        log.emit(json!({
                            "kind": "race_timeout",
                            "origin_id": p.origin_id,
                            "target_id": cfg.targets[p.target_idx].id,
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
                let quotes: Vec<Value> = cfg.targets.iter().map(|t| {
                    let (b, a) = last_quotes.get(&t.token_id).copied().unwrap_or((None, None));
                    let book = books.get(&t.token_id);
                    let health = book.map_or("Missing", |x| x.health().as_str());
                    let age_ms = book.and_then(|x| x.last_update()).map(|u| u.elapsed().as_millis());
                    json!({"target_id": t.id, "best_bid": b, "best_ask": a, "book_health": health, "book_age_ms": age_ms})
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

/// Called after every applied book update: logs best-quote moves and resolves
/// pending races (origin flip happened; did/when did the book move?).
fn on_quotes(
    log: &Jsonl,
    cfg: &Config,
    pending: &mut Vec<Pending>,
    last_quotes: &mut HashMap<String, (Option<f64>, Option<f64>)>,
    asset_id: &str,
    tracker: &BookTracker,
    recv_at: Instant,
) {
    let bid = tracker.best_bid().map(|q| q.0);
    let ask = tracker.best_ask().map(|q| q.0);
    let prev = last_quotes.insert(asset_id.to_string(), (bid, ask));
    let changed = prev.map_or(true, |(b, a)| b != bid || a != ask);
    if !changed {
        return;
    }
    log.emit(json!({
        "kind": "book_change",
        "token_id": asset_id,
        "best_bid": bid,
        "best_ask": ask,
        "book_health": tracker.health().as_str(),
        "recv_ms": wall_of(recv_at),
    }));
    // Resolve any pending race for targets bound to this token (first
    // material move). Forward-walk sampling continues regardless — the
    // verdict comes from the fixed-offset samples, not the first move.
    let mut i = 0;
    while i < pending.len() {
        let matches_token = !pending[i].resolved
            && cfg.targets[pending[i].target_idx].token_id == asset_id;
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