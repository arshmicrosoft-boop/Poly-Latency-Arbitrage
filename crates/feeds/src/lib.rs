//! Origin data-source adapters. The only adapter in v1 is `HttpPoller`:
//! conditional-GET polling (ETag / If-Modified-Since) of a public endpoint,
//! with a body-hash fallback for servers that send no cache headers.
//!
//! Deliberately polite defaults: single IP, no proxy rotation, floor of
//! 200 ms between polls. Keep it that way while measuring; only revisit
//! polling cadence if the dry-run data shows the origin itself is the lag.

use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct OriginEvent {
    pub at: SystemTime,
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

pub struct HttpPoller {
    client: reqwest::Client,
    pub url: String,
    pub interval: Duration,
    etag: Option<String>,
    last_modified: Option<String>,
    last_hash: Option<String>,
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
        }
    }

    pub async fn poll_once(&mut self) -> Result<OriginEvent> {
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
        let status = resp.status().as_u16();

        if status == 304 {
            return Ok(OriginEvent {
                at: SystemTime::now(),
                url: self.url.clone(),
                status,
                changed: false,
                etag: self.etag.clone(),
                last_modified: self.last_modified.clone(),
                body_sha256: self.last_hash.clone(),
                body_len: 0,
            });
        }

        let etag = resp
            .headers()
            .get("etag")
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        let last_modified = resp
            .headers()
            .get("last-modified")
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        let body = resp.bytes().await.context("read origin body")?;
        let hash = hex::encode(Sha256::digest(&body));
        let changed = is_change(self.last_hash.as_deref(), &hash);
        self.etag = etag.clone();
        self.last_modified = last_modified.clone();
        self.last_hash = Some(hash.clone());

        Ok(OriginEvent {
            at: SystemTime::now(),
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
    }
}