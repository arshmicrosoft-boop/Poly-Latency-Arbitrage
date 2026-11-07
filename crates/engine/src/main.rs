//! cashcat — dry-run latency-arbitrage measurement harness.
//!
//! READ-ONLY BY DESIGN: subscribes to the public Polymarket book feed and
//! polls public origin endpoints; logs JSONL; never holds keys and has no
//! order-placement code path. If the logs never show a tradeable window,
//! that is the answer, and nothing here should grow an execution path.

mod book;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use book::BookTracker;
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

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn ms_of(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

/// Append-only JSONL logger. One line per event, flushed immediately so a
/// crash never loses the tail of a session.
struct Jsonl {
    inner: std::sync::Mutex<std::io::BufWriter<std::fs::File>>,
}

impl Jsonl {
    fn create(path: &std::path::Path) -> Result<Self> {
        let f = std::fs::File::create(path)
            .with_context(|| format!("create {}", path.display()))?;
        Ok(Self {
            inner: std::sync::Mutex::new(std::io::BufWriter::new(f)),
        })
    }
    fn emit(&self, mut v: Value) {
        if let Some(o) = v.as_object_mut() {
            o.insert("ts_ms".into(), json!(now_ms()));
        }
        if let Ok(mut w) = self.inner.lock() {
            let _ = writeln!(w, "{}", v);
            let _ = w.flush();
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
        Cmd::List { max_pages, regex, url_only } => list(max_pages, regex, url_only).await,
        Cmd::Watch { config } => watch(config).await,
    }
}

async fn list(max_pages: u32, regex: Option<String>, url_only: bool) -> Result<()> {
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
            println!(
                "{}\t{}\t{}\t{}",
                m.market_slug,
                m.end_date_iso.clone().unwrap_or_default(),
                m.question,
                if has_url { "url-src" } else { "" }
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
    target_id: String,
    t0: u128,
    ask_before: Option<f64>,
    bid_before: Option<f64>,
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
    let log_path = std::path::Path::new(&cfg.data_dir).join(format!("race-{}.jsonl", now_ms()));
    let log = std::sync::Arc::new(Jsonl::create(&log_path)?);
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

    // WS feed task with reconnect/backoff.
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
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    });

    // One poller task per target.
    let (origin_tx, mut origin_rx) = tokio::sync::mpsc::channel::<(usize, feeds::OriginEvent)>(256);
    for (i, t) in cfg.targets.iter().enumerate() {
        let mut poller = feeds::HttpPoller::new(
            client.clone(),
            t.origin.url.clone(),
            Duration::from_millis(t.origin.interval_ms),
        );
        let tx = origin_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(poller.interval).await;
                match poller.poll_once().await {
                    Ok(ev) => {
                        if tx.send((i, ev)).await.is_err() {
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
        "targets": cfg.targets.iter().map(|t| json!({
            "id": t.id, "token_id": t.token_id, "origin": t.origin.url,
            "interval_ms": t.origin.interval_ms, "note": t.note
        })).collect::<Vec<_>>(),
    }));

    let mut pending: Vec<Pending> = Vec::new();
    let mut origin_seq: u64 = 0;
    let window_ms: u128 = cfg.window_s as u128 * 1000;
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
                let Some(clob::ws::FeedMsg { recv_at, event }) = msg else { break };
                match event {
                    clob::ws::WsEvent::Book { asset_id, bids, asks } => {
                        let tracker = books.entry(asset_id.clone()).or_default();
                        tracker.load_snapshot(&bids, &asks);
                        on_quotes(&log, &cfg, &mut pending, &mut last_quotes, &asset_id, tracker, ms_of(recv_at));
                    }
                    clob::ws::WsEvent::PriceChange { asset_id, changes } => {
                        if let Some(tracker) = books.get_mut(&asset_id) {
                            for c in changes {
                                tracker.apply_change(c.side, c.price, c.size);
                            }
                            on_quotes(&log, &cfg, &mut pending, &mut last_quotes, &asset_id, tracker, ms_of(recv_at));
                        }
                    }
                    clob::ws::WsEvent::LastTrade { .. } => {}
                    clob::ws::WsEvent::Other(_) => {}
                }
            }
            msg = origin_rx.recv() => {
                let Some((i, ev)) = msg else { break };
                if !ev.changed {
                    continue; // 304 / identical body: nothing to race on
                }
                origin_seq += 1;
                let target = &cfg.targets[i];
                log.emit(json!({
                    "kind": "origin_event",
                    "origin_id": origin_seq,
                    "target_id": target.id,
                    "status": ev.status,
                    "body_len": ev.body_len,
                    "body_sha256": ev.body_sha256,
                    "etag": ev.etag,
                }));
                let (bid, ask) = books
                    .get(&target.token_id)
                    .map(|b| (b.best_bid().map(|q| q.0), b.best_ask().map(|q| q.0)))
                    .unwrap_or((None, None));
                pending.push(Pending {
                    origin_id: origin_seq,
                    target_id: target.id.clone(),
                    t0: ms_of(ev.at),
                    ask_before: ask,
                    bid_before: bid,
                });
            }
            _ = tick.tick() => {
                // Time out races that saw no book move within the window.
                let now = now_ms();
                let mut i = 0;
                while i < pending.len() {
                    if now - pending[i].t0 >= window_ms {
                        let p = pending.remove(i);
                        log.emit(json!({
                            "kind": "race_timeout",
                            "origin_id": p.origin_id,
                            "target_id": p.target_id,
                            "window_ms": window_ms,
                            "ask_before": p.ask_before,
                            "bid_before": p.bid_before,
                        }));
                    } else {
                        i += 1;
                    }
                }
            }
            _ = heartbeat.tick() => {
                let quotes: Vec<Value> = cfg.targets.iter().map(|t| {
                    let (b, a) = last_quotes.get(&t.token_id).copied().unwrap_or((None, None));
                    json!({"target_id": t.id, "best_bid": b, "best_ask": a})
                }).collect();
                log.emit(json!({"kind": "heartbeat", "quotes": quotes, "pending_races": pending.len()}));
            }
        }
    }
    log.emit(json!({"kind": "session_end"}));
    Ok(())
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
    recv_ms: u128,
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
        "recv_ms": recv_ms,
    }));
    // Resolve any pending race for targets bound to this token.
    let mut i = 0;
    while i < pending.len() {
        let matches_token = cfg
            .targets
            .iter()
            .any(|t| t.id == pending[i].target_id && t.token_id == asset_id);
        if matches_token {
            let p = pending.remove(i);
            log.emit(json!({
                "kind": "race",
                "origin_id": p.origin_id,
                "target_id": p.target_id,
                "token_id": asset_id,
                "latency_ms": recv_ms.saturating_sub(p.t0),
                "ask_before": p.ask_before,
                "ask_after": ask,
                "bid_before": p.bid_before,
                "bid_after": bid,
            }));
        } else {
            i += 1;
        }
    }
}