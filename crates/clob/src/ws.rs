//! WSS market-channel feed (read-only). Parses `book`, `price_change` and
//! `last_trade_price` events. Timestamps are taken at message receipt so the
//! engine can measure origin-flip -> book-move latency.

use std::time::SystemTime;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::debug;

pub struct FeedMsg {
    pub recv_at: SystemTime,
    pub event: WsEvent,
}

#[derive(Debug, Clone)]
pub enum WsEvent {
    Book {
        asset_id: String,
        bids: Vec<(f64, f64)>,
        asks: Vec<(f64, f64)>,
    },
    PriceChange {
        asset_id: String,
        changes: Vec<LevelChange>,
    },
    LastTrade {
        asset_id: String,
        price: f64,
    },
    /// Unrecognized event types kept for debug logging.
    Other(Value),
}

#[derive(Debug, Clone)]
pub struct LevelChange {
    pub side: Side,
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
}

fn f64_of(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn field_f64(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(f64_of)
}

fn parse_levels(v: &Value, key: &str) -> Vec<(f64, f64)> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| Some((field_f64(l, "price")?, field_f64(l, "size")?)))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_single_change(v: &Value) -> Option<LevelChange> {
    let side = match v.get("side").and_then(|s| s.as_str()) {
        Some("BUY") => Side::Bid,
        Some("SELL") => Side::Ask,
        _ => return None,
    };
    Some(LevelChange {
        side,
        price: field_f64(v, "price")?,
        size: field_f64(v, "size")?,
    })
}

/// Parse one WS JSON frame. Schema drift is tolerated: unknown shapes become
/// `WsEvent::Other` rather than errors so logging never stops.
pub fn parse_event(v: Value) -> WsEvent {
    let et = v.get("event_type").and_then(|x| x.as_str()).unwrap_or("");
    let asset = v
        .get("asset_id")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    match et {
        "book" => WsEvent::Book {
            asset_id: asset,
            bids: parse_levels(&v, "bids"),
            asks: parse_levels(&v, "asks"),
        },
        "price_change" => {
            let mut changes: Vec<LevelChange> = v
                .get("changes")
                .and_then(|x| x.as_array())
                .map(|arr| arr.iter().filter_map(parse_single_change).collect())
                .unwrap_or_default();
            // Legacy single-change shape: fields at top level.
            if changes.is_empty() {
                if let Some(c) = parse_single_change(&v) {
                    changes.push(c);
                }
            }
            WsEvent::PriceChange {
                asset_id: asset,
                changes,
            }
        }
        "last_trade_price" => WsEvent::LastTrade {
            asset_id: asset,
            price: field_f64(&v, "price").unwrap_or(0.0),
        },
        _ => WsEvent::Other(v),
    }
}

/// Connect, subscribe to the market channel for `token_ids`, and pump events
/// into `tx` until the socket closes or the receiver is dropped. Reconnection
/// is the caller's job (see engine).
pub async fn run_market_feed(token_ids: Vec<String>, tx: mpsc::Sender<FeedMsg>) -> Result<()> {
    let (sock, _resp) =
        tokio_tungstenite::connect_async(crate::WS_MARKET)
            .await
            .context("ws connect")?;
    let (mut write, mut read) = sock.split();
    let sub = json!({"type": "market", "assets_ids": token_ids});
    write
        .send(Message::Text(sub.to_string()))
        .await
        .context("ws subscribe")?;

    while let Some(msg) = read.next().await {
        let recv_at = SystemTime::now();
        let msg = msg.context("ws message")?;
        match msg {
            Message::Text(t) => {
                let v: Value = match serde_json::from_str(&t) {
                    Ok(v) => v,
                    Err(_) => {
                        debug!(raw = %t, "non-JSON ws frame");
                        continue;
                    }
                };
                // Server sometimes sends a JSON array of events in one frame.
                let items: Vec<Value> = if v.is_array() {
                    v.as_array().unwrap().clone()
                } else {
                    vec![v]
                };
                for item in items {
                    let event = parse_event(item);
                    if tx.send(FeedMsg { recv_at, event }).await.is_err() {
                        return Ok(()); // receiver gone
                    }
                }
            }
            Message::Ping(p) => {
                write.send(Message::Pong(p)).await.context("ws pong")?;
            }
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_book_frame() {
        let v: Value = serde_json::from_str(
            r#"{"event_type":"book","asset_id":"777","market":"0xabc",
                "bids":[{"price":"0.60","size":"100"},{"price":"0.59","size":"40"}],
                "asks":[{"price":"0.63","size":"25"}],"timestamp":"123"}"#,
        )
        .unwrap();
        match parse_event(v) {
            WsEvent::Book { asset_id, bids, asks } => {
                assert_eq!(asset_id, "777");
                assert_eq!(bids, vec![(0.60, 100.0), (0.59, 40.0)]);
                assert_eq!(asks, vec![(0.63, 25.0)]);
            }
            other => panic!("expected book, got {other:?}"),
        }
    }

    #[test]
    fn parse_price_change_batch_and_legacy() {
        let v: Value = serde_json::from_str(
            r#"{"event_type":"price_change","asset_id":"777",
                "changes":[{"price":"0.62","side":"SELL","size":"0"},
                           {"price":"0.64","side":"SELL","size":"12"}]}"#,
        )
        .unwrap();
        match parse_event(v) {
            WsEvent::PriceChange { changes, .. } => {
                assert_eq!(changes.len(), 2);
                assert_eq!(changes[0].side, Side::Ask);
                assert_eq!(changes[0].size, 0.0);
            }
            other => panic!("expected price_change, got {other:?}"),
        }

        let legacy: Value = serde_json::from_str(
            r#"{"event_type":"price_change","asset_id":"777","price":"0.5","side":"BUY","size":"9"}"#,
        )
        .unwrap();
        match parse_event(legacy) {
            WsEvent::PriceChange { changes, .. } => {
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].side, Side::Bid);
            }
            other => panic!("expected legacy price_change, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_becomes_other() {
        let v: Value = serde_json::from_str(r#"{"event_type":"tick_size_change","x":1}"#).unwrap();
        assert!(matches!(parse_event(v), WsEvent::Other(_)));
    }
}