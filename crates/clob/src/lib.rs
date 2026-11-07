//! Read-only Polymarket CLOB client: market discovery + order-book snapshots.
//!
//! Decision (2026-11-07): discovery via REST `/markets` + `/sampling-markets`,
//! live quotes via the WSS market channel (`ws.rs`). This crate has no auth,
//! no signing and no order placement on purpose: the harness is dry-run only.
//! rustls everywhere so Windows builds need no OpenSSL.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub mod ws;

pub const CLOB_BASE: &str = "https://clob.polymarket.com";
pub const GAMMA_BASE: &str = "https://gamma-api.polymarket.com";
pub const WS_MARKET: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("cashcat-dryrun/0.1 (read-only research)")
        .timeout(Duration::from_secs(15))
        .build()
        .context("build http client")
}

/// Polymarket is not consistent: some token fields arrive as JSON strings,
/// others as bare numbers (`"price": 0` on dead markets). Lenient
/// deserializers so one odd market cannot kill a 500-doc page.
mod lenient {
    use serde::{Deserialize, Deserializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        S(String),
        N(f64),
        B(bool),
    }

    impl V {
        fn into_string(self) -> String {
            match self {
                V::S(s) => s,
                V::N(n) => n.to_string(),
                V::B(b) => b.to_string(),
            }
        }
    }

    pub fn string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        V::deserialize(d).map(V::into_string)
    }

    pub fn opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        Ok(Option::<V>::deserialize(d)?.map(V::into_string))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketToken {
    #[serde(default, deserialize_with = "lenient::string")]
    pub token_id: String,
    #[serde(default, deserialize_with = "lenient::string")]
    pub outcome: String,
    #[serde(default, deserialize_with = "lenient::opt_string")]
    pub price: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketDoc {
    #[serde(default)]
    pub condition_id: String,
    #[serde(default)]
    pub question_id: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub market_slug: String,
    #[serde(default)]
    pub end_date_iso: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub closed: bool,
    #[serde(default)]
    pub accepting_orders: bool,
    #[serde(default)]
    pub minimum_tick_size: f64,
    #[serde(default)]
    pub minimum_order_size: f64,
    #[serde(default)]
    pub tokens: Vec<MarketToken>,
}

#[derive(Debug, Clone)]
pub struct Page {
    pub data: Vec<MarketDoc>,
    pub next_cursor: String,
}

#[derive(Deserialize)]
struct PageRaw {
    #[serde(default)]
    data: Vec<MarketDoc>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// One page of CLOB markets. Pass `cursor=""` for the first page.
/// `next_cursor` of `""` or `"LTE="` (base64 of "-1") means end of list.
///
/// Note: `/markets` walks from the OLDEST markets (mostly closed), which is
/// useless for discovery. `/sampling-markets` returns currently-active,
/// accepting markets in the same document shape — use that for discovery.
pub async fn list_markets_page(client: &reqwest::Client, cursor: &str) -> Result<Page> {
    page(client, "markets", cursor).await
}

/// Active markets only (accepting_orders=true implied by the endpoint).
pub async fn list_sampling_page(client: &reqwest::Client, cursor: &str) -> Result<Page> {
    page(client, "sampling-markets", cursor).await
}

async fn page(client: &reqwest::Client, endpoint: &str, cursor: &str) -> Result<Page> {
    let mut url = format!("{CLOB_BASE}/{endpoint}");
    if !cursor.is_empty() {
        url.push_str(&format!("?next_cursor={cursor}"));
    }
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET /{endpoint}"))?
        .error_for_status()?;
    let raw: PageRaw = resp
        .json()
        .await
        .with_context(|| format!("parse /{endpoint} page"))?;
    Ok(Page {
        data: raw.data,
        next_cursor: raw.next_cursor.unwrap_or_default(),
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Book {
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

impl Book {
    pub fn best_bid(&self) -> Option<BookLevel> {
        self.bids
            .iter()
            .max_by(|a, b| a.price.partial_cmp(&b.price).unwrap())
            .copied()
    }
    pub fn best_ask(&self) -> Option<BookLevel> {
        self.asks
            .iter()
            .min_by(|a, b| a.price.partial_cmp(&b.price).unwrap())
            .copied()
    }
}

#[derive(Deserialize)]
struct RawLevel {
    price: String,
    size: String,
}

#[derive(Deserialize)]
struct RawBook {
    #[serde(default)]
    bids: Vec<RawLevel>,
    #[serde(default)]
    asks: Vec<RawLevel>,
}

/// REST snapshot of one token's book. Used for (re)initialization only;
/// the live harness tracks levels from the WS stream afterwards.
pub async fn get_book(client: &reqwest::Client, token_id: &str) -> Result<Book> {
    let resp = client
        .get(format!("{CLOB_BASE}/book"))
        .query(&[("token_id", token_id)])
        .send()
        .await
        .context("GET /book")?
        .error_for_status()?;
    let raw: RawBook = resp.json().await.context("parse /book")?;
    let parse = |ls: Vec<RawLevel>| -> Vec<BookLevel> {
        ls.into_iter()
            .filter_map(|l| {
                Some(BookLevel {
                    price: l.price.parse().ok()?,
                    size: l.size.parse().ok()?,
                })
            })
            .collect()
    };
    Ok(Book {
        bids: parse(raw.bids),
        asks: parse(raw.asks),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MARKET: &str = r#"{
        "enable_order_book": true,
        "active": true,
        "closed": false,
        "accepting_orders": true,
        "minimum_order_size": 5,
        "minimum_tick_size": 0.001,
        "condition_id": "0xa467",
        "question_id": "0x1d92",
        "question": "Xi Jinping out before 2027?",
        "description": "resolves via credible reporting",
        "market_slug": "xi-jinping-out-before-2027",
        "tokens": [
            {"token_id": "111", "outcome": "Yes", "price": "0.03"},
            {"token_id": "222", "outcome": "No", "price": "0.97"}
        ]
    }"#;

    #[test]
    fn parse_market_doc() {
        let m: MarketDoc = serde_json::from_str(SAMPLE_MARKET).unwrap();
        assert_eq!(m.market_slug, "xi-jinping-out-before-2027");
        assert!(m.active && !m.closed);
        assert_eq!(m.tokens.len(), 2);
        assert_eq!(m.tokens[0].outcome, "Yes");
    }

    #[test]
    fn token_fields_as_numbers() {
        // Live API sends `"price": 0` (integer) on dead markets and
        // occasionally numeric token_ids; must not kill page parsing.
        let t: MarketToken =
            serde_json::from_str(r#"{"token_id": 123, "outcome": "Yes", "price": 0}"#).unwrap();
        assert_eq!(t.token_id, "123");
        assert_eq!(t.price.as_deref(), Some("0"));
    }

    #[test]
    fn best_quotes() {
        let book = Book {
            bids: vec![
                BookLevel { price: 0.60, size: 100.0 },
                BookLevel { price: 0.62, size: 50.0 },
                BookLevel { price: 0.58, size: 200.0 },
            ],
            asks: vec![
                BookLevel { price: 0.66, size: 80.0 },
                BookLevel { price: 0.64, size: 30.0 },
            ],
        };
        assert_eq!(book.best_bid().unwrap().price, 0.62);
        assert_eq!(book.best_ask().unwrap().price, 0.64);
        assert!(Book::default().best_ask().is_none());
    }
}