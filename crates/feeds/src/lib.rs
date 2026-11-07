//! Origin data-source adapters. The only adapter in v1 is `HttpPoller`:
//! conditional-GET polling (ETag / If-Modified-Since) of a public endpoint,
//! with a body-hash fallback for servers that send no cache headers.
//!
//! Deliberately polite defaults: single IP, no proxy rotation, floor of
//! 200 ms between polls. Keep it that way while measuring; only revisit
//! polling cadence if the dry-run data shows the origin itself is the lag.
//!
//! Measurement-spec features (docs/MEASUREMENT-SPEC.md §1, §3):
//! - per-poll monotonic stage breakpoints: scheduled → req sent → headers
//!   rx → body rx → parse done
//! - publication uncertainty window: `W = t_headers_rx(this poll) −
//!   t_req_sent(previous poll)`; the true mutation time lies inside it
//! - CDN cache-state capture (`CF-Cache-Status` / `X-Cache`, `Age`, `Date`):
//!   an `Age > 0` or cache HIT during a release window means the measurement
//!   is of a stale ghost response and must be flagged.

use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct OriginEvent {
    /// Wall-clock receipt time — correlation/logging only, never latency math.
    pub at: SystemTime,
    // Monotonic stage breakpoints (spec §3).
    pub t_scheduled: Instant,
    pub t_req_sent: Instant,
    pub t_headers_rx: Instant,
    pub t_body_rx: Instant,
    pub t_parse_done: Instant,
    /// Publication uncertainty window in ms: headers-rx of this poll minus
    /// req-sent of the previous poll. `None` for the first poll (baseline).
    /// Conservative analysis assumes the origin changed at the START of the
    /// window.
    pub uncertainty_ms: Option<u128>,
    // CDN cache state (spec "Origin probe requirements").
    pub cf_cache_status: Option<String>,
    pub x_cache: Option<String>,
    pub age: Option<String>,
    pub http_date: Option<String>,
    pub url: String,
    pub status: u16,
    /// True when the response body differs from the previous poll (or this is
    /// the first poll, which establishes the baseline).
    pub changed: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub body_sha256: Option<String>,
    pub body_len: usize,
}

pub fn is_change(prev_hash: Option<&str>, new_hash: &str) -> bool {
    prev_hash.map_or(true, |p| p != new_hash)
}

fn hdr(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
}

pub struct HttpPoller {
    client: reqwest::Client,
    pub url: String,
    pub interval: Duration,
    etag: Option<String>,
    last_modified: Option<String>,
    last_hash: Option<String>,
    /// req-sent time of the previous poll; with this poll's headers-rx it
    /// bounds when the origin actually mutated (spec §1).
    prev_req_sent: Option<Instant>,
}

impl HttpPoller {
    pub fn new(client: reqwest::Client, url: String, interval: Duration) -> Self {
        Self {
            client,
            url,
            interval: interval.max(Duration::from_millis(200)),
            etag: None,
            last_modified: None,
            last_hash: None,
            prev_req_sent: None,
        }
    }

    pub async fn poll_once(&mut self, scheduled_at: Instant) -> Result<OriginEvent> {
        let t_req_sent = Instant::now();
        let mut req = self.client.get(&self.url);
        if let Some(e) = &self.etag {
            req = req.header("If-None-Match", e.clone());
        }
        if let Some(lm) = &self.last_modified {
            req = req.header("If-Modified-Since", lm.clone());
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("poll {}", self.url))?;
        // reqwest returns once response headers are available.
        let t_headers_rx = Instant::now();
        let uncertainty_ms = self
            .prev_req_sent
            .map(|p| t_headers_rx.duration_since(p).as_millis());
        self.prev_req_sent = Some(t_req_sent);

        let status = resp.status().as_u16();
        let cf_cache_status = hdr(&resp, "cf-cache-status");
        let x_cache = hdr(&resp, "x-cache");
        let age = hdr(&resp, "age");
        let http_date = hdr(&resp, "date");

        if status == 304 {
            return Ok(OriginEvent {
                at: SystemTime::now(),
                t_scheduled: scheduled_at,
                t_req_sent,
                t_headers_rx,
                t_body_rx: t_headers_rx,
                t_parse_done: t_headers_rx,
                uncertainty_ms,
                cf_cache_status,
                x_cache,
                age,
                http_date,
                url: self.url.clone(),
                status,
                changed: false,
                etag: self.etag.clone(),
                last_modified: self.last_modified.clone(),
                body_sha256: self.last_hash.clone(),
                body_len: 0,
            });
        }

        let etag = hdr(&resp, "etag");
        let last_modified = hdr(&resp, "last-modified");
        let body = resp.bytes().await.context("read origin body")?;
        let t_body_rx = Instant::now();
        let hash = hex::encode(Sha256::digest(&body));
        let t_parse_done = Instant::now();
        let changed = is_change(self.last_hash.as_deref(), &hash);
        self.etag = etag.clone();
        self.last_modified = last_modified.clone();
        self.last_hash = Some(hash.clone());

        Ok(OriginEvent {
            at: SystemTime::now(),
            t_scheduled: scheduled_at,
            t_req_sent,
            t_headers_rx,
            t_body_rx,
            t_parse_done,
            uncertainty_ms,
            cf_cache_status,
            x_cache,
            age,
            http_date,
            url: self.url.clone(),
            status,
            changed,
            etag,
            last_modified,
            body_sha256: Some(hash),
            body_len: body.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_detection() {
        assert!(is_change(None, "aa")); // first poll is a baseline "change"
        assert!(!is_change(Some("aa"), "aa"));
        assert!(is_change(Some("aa"), "bb"));
    }

    #[test]
    fn interval_floor() {
        let p = HttpPoller::new(
            reqwest::Client::new(),
            "https://example.com".into(),
            Duration::from_millis(1),
        );
        assert_eq!(p.interval, Duration::from_millis(200));
        assert!(p.prev_req_sent.is_none());
    }
}