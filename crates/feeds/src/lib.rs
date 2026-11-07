//! Origin data-source adapters. `HttpPoller` does conditional-GET polling
//! (ETag / If-Modified-Since) of a public endpoint, with a body-hash fallback
//! for servers that send no cache headers.
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
//!
//! Semantic extractors (pass 3): structured feeds carry a monotonic signal
//! (daily max magnitude, tweet counter). `Extractor` maps a body to that
//! signal value so the engine only opens races on bin-relevant changes
//! instead of every body-hash tick (metadata timestamps, sub-threshold
//! events). Extraction runs on the engine side of the poll; the transport
//! here still records the hash for release-completeness logging.

use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Maps an origin body to a monotonic signal value.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Extractor {
    /// Fallback for unstructured feeds (e.g. NOAA plain text): the engine
    /// fires on body-SHA256 change instead of a signal value.
    RawHash,
    /// USGS GeoJSON feed: signal = max `features[].properties.mag` rounded
    /// to one decimal (Polymarket brackets resolve on the reported 1-dp
    /// magnitude), or 0.0 when nothing passes the filters (no events yet —
    /// below every market bin). Features outside the optional UTC time
    /// window (`properties.time`, compared on the 19-char prefix) or below
    /// `min_mag` are ignored; the same filters usually also live in the
    /// query URL, client-side filtering is defense in depth against cached
    /// responses drifting past the window. New events smaller than the
    /// running max do not move the signal (engine gates on increase).
    // serde's snake_case splits "GeoJson" into "geo_json"; pin the tag
    // configs actually use.
    #[serde(rename = "usgs_geojson_max_mag")]
    UsgsGeoJsonMaxMag {
        #[serde(default)]
        start_time_utc: Option<String>,
        #[serde(default)]
        end_time_utc: Option<String>,
        #[serde(default)]
        min_mag: Option<f64>,
    },
    /// RFC 6901 JSON Pointer to a numeric counter (e.g. `/tweetCount`).
    JsonPointerNumber { pointer: String },
}

impl Default for Extractor {
    fn default() -> Self {
        Extractor::RawHash
    }
}

/// Parse a "YYYY-MM-DDTHH:MM:SS" UTC timestamp (19-char prefix used; any
/// fractional seconds / zone suffix ignored) into epoch seconds. Days math
/// is Howard Hinnant's civil_from_days, mirrored by the engine's utc_stamp.
fn parse_utc_secs(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |lo: usize, hi: usize| -> Option<i64> {
        let mut n = 0i64;
        for &c in &b[lo..hi] {
            if !c.is_ascii_digit() {
                return None;
            }
            n = n * 10 + (c - b'0') as i64;
        }
        Some(n)
    };
    let (y, mo, d, h, mi, se) = (
        num(0, 4)?,
        num(5, 7)?,
        num(8, 10)?,
        num(11, 13)?,
        num(14, 16)?,
        num(17, 19)?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = (if y2 >= 0 { y2 } else { y2 - 399 }) / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + se)
}

impl Extractor {
    /// The signal value for this body, or `None` when no signal can be
    /// extracted. `RawHash` never yields a value (the engine falls back to
    /// hash comparison); a parse failure also yields `None` — the engine
    /// must not open races on garbage it cannot read.
    pub fn extract(&self, body: &[u8]) -> Option<f64> {
        match self {
            Extractor::RawHash => None,
            Extractor::UsgsGeoJsonMaxMag {
                start_time_utc,
                end_time_utc,
                min_mag,
            } => {
                let v: Value = serde_json::from_slice(body).ok()?;
                let feats = v.get("features")?.as_array()?;
                // Window bounds as epoch seconds; `None` = unbounded side.
                let lo = start_time_utc.as_deref().and_then(parse_utc_secs);
                let hi = end_time_utc.as_deref().and_then(parse_utc_secs);
                let in_window = |f: &Value| -> bool {
                    // Observed live (2026-11-07): properties.time is an
                    // epoch-milliseconds NUMBER, though the FDSN docs show
                    // a "YYYY-MM-DDTHH:MM:SS.mmmZ" string. Accept both.
                    let t_secs: Option<f64> = match f.pointer("/properties/time") {
                        Some(Value::Number(n)) => n.as_f64().map(|ms| ms / 1000.0),
                        Some(Value::String(s)) => parse_utc_secs(s).map(|s| s as f64),
                        _ => None,
                    };
                    let Some(t) = t_secs else {
                        // A window is configured but the timestamp is
                        // unreadable: exclude rather than guess.
                        return lo.is_none() && hi.is_none();
                    };
                    if let Some(s) = lo {
                        if t < s as f64 {
                            return false;
                        }
                    }
                    if let Some(e) = hi {
                        if t >= e as f64 {
                            return false;
                        }
                    }
                    true
                };
                let mut max = 0.0f64;
                for f in feats {
                    if !in_window(f) {
                        continue;
                    }
                    if let Some(m) = f.pointer("/properties/mag").and_then(Value::as_f64) {
                        if min_mag.map_or(false, |lo| m < lo) {
                            continue;
                        }
                        let r = (m * 10.0).round() / 10.0;
                        if r > max {
                            max = r;
                        }
                    }
                }
                Some(max)
            }
            Extractor::JsonPointerNumber { pointer } => {
                let v: Value = serde_json::from_slice(body).ok()?;
                v.pointer(pointer).and_then(Value::as_f64)
            }
        }
    }
}

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
    /// Response body (empty for 304s). Kept so the engine can run semantic
    /// extractors; moved, not cloned, through the channel.
    pub body: Vec<u8>,
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
    /// URL of the previous poll. Conditional headers are only sent when the
    /// URL repeats — an If-Modified-Since captured from a different resource
    /// (e.g. a previous `endtime` bucket) is meaningless.
    last_url: Option<String>,
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
            last_url: None,
            prev_req_sent: None,
        }
    }

    pub async fn poll_once(&mut self, scheduled_at: Instant) -> Result<OriginEvent> {
        let url = self.url.clone();
        self.poll_url(scheduled_at, &url).await
    }

    /// Poll an explicit URL. Used for cache-busting templates whose URL
    /// changes every tick (e.g. FDSN `endtime={now_utc}` behind CloudFront,
    /// where repeat polls of an identical URL return `X-Cache: Hit` with a
    /// stale body).
    pub async fn poll_url(&mut self, scheduled_at: Instant, url: &str) -> Result<OriginEvent> {
        let same_url = self.last_url.as_deref() == Some(url);
        self.last_url = Some(url.to_string());
        let t_req_sent = Instant::now();
        let mut req = self.client.get(url);
        if same_url {
            if let Some(e) = &self.etag {
                req = req.header("If-None-Match", e.clone());
            }
            if let Some(lm) = &self.last_modified {
                req = req.header("If-Modified-Since", lm.clone());
            }
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("poll {url}"))?;
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
                url: url.to_string(),
                status,
                changed: false,
                etag: self.etag.clone(),
                last_modified: self.last_modified.clone(),
                body_sha256: self.last_hash.clone(),
                body_len: 0,
                body: Vec::new(),
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
            url: url.to_string(),
            status,
            changed,
            etag,
            last_modified,
            body_sha256: Some(hash),
            body_len: body.len(),
            body: body.to_vec(),
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

    fn max_mag(min_mag: Option<f64>, start: Option<&str>, end: Option<&str>) -> Extractor {
        Extractor::UsgsGeoJsonMaxMag {
            start_time_utc: start.map(String::from),
            end_time_utc: end.map(String::from),
            min_mag,
        }
    }

    #[test]
    fn usgs_max_mag() {
        let ex = max_mag(None, None, None);
        let body = br#"{"features":[
            {"properties":{"mag":4.3}},
            {"properties":{"mag":5.1}},
            {"properties":{"mag":4.9}}
        ]}"#;
        assert_eq!(ex.extract(body), Some(5.1));
        // Empty day: below every bin, not "no signal".
        assert_eq!(ex.extract(br#"{"features":[]}"#), Some(0.0));
        // Null mags are skipped, not fatal.
        assert_eq!(
            ex.extract(br#"{"features":[{"properties":{"mag":null}},{"properties":{"mag":4.6}}]}"#),
            Some(4.6)
        );
        // Garbage body: no signal, engine must not race.
        assert_eq!(ex.extract(b"<html>oops</html>"), None);
        assert_eq!(ex.extract(br#"{"no_features":1}"#), None);
    }

    #[test]
    fn usgs_max_mag_rounds_to_one_decimal() {
        // Polymarket brackets resolve on the reported 1-dp magnitude;
        // 5.35 must route as 5.4 (bin [5.3, 5.5)), 5.67 as 5.7.
        let ex = max_mag(None, None, None);
        assert_eq!(
            ex.extract(br#"{"features":[{"properties":{"mag":5.35}}]}"#),
            Some(5.4)
        );
        assert_eq!(
            ex.extract(br#"{"features":[{"properties":{"mag":5.67}}]}"#),
            Some(5.7)
        );
        assert_eq!(
            ex.extract(br#"{"features":[{"properties":{"mag":5.31}},{"properties":{"mag":5.29}}]}"#),
            Some(5.3)
        );
    }

    #[test]
    fn usgs_max_mag_filters_window_and_min_mag() {
        let body = br#"{"features":[
            {"properties":{"mag":6.5,"time":"2026-11-06T23:50:00.000Z"}},
            {"properties":{"mag":4.2,"time":"2026-11-07T01:00:00.000Z"}},
            {"properties":{"mag":5.3,"time":"2026-11-07T02:00:00.000Z"}},
            {"properties":{"mag":5.9,"time":"2026-11-08T00:30:00.000Z"}}
        ]}"#;
        let ex = max_mag(
            Some(4.0),
            Some("2026-11-07T00:00:00"),
            Some("2026-11-08T00:00:00"),
        );
        // Yesterday's 6.5 and tomorrow's 5.9 are outside the UTC day;
        // 4.2 passes min_mag but is not the max.
        assert_eq!(ex.extract(body), Some(5.3));
        // min_mag filter: nothing >= 5.5 inside the window -> empty-day 0.0.
        let picky = max_mag(
            Some(5.5),
            Some("2026-11-07T00:00:00"),
            Some("2026-11-08T00:00:00"),
        );
        assert_eq!(picky.extract(body), Some(0.0));
        // No window: global max including out-of-day events.
        assert_eq!(max_mag(None, None, None).extract(body), Some(6.5));
    }

    #[test]
    fn usgs_max_mag_handles_epoch_ms_timestamps() {
        // Live FDSN (observed 2026-11-07) returns properties.time as an
        // epoch-milliseconds NUMBER, not the documented ISO string.
        // Window 2026-11-07 = [1794009600, 1794096000) epoch seconds.
        let body = br#"{"features":[
            {"properties":{"mag":6.5,"time":1794009599000}},
            {"properties":{"mag":4.2,"time":1794013200000}},
            {"properties":{"mag":5.3,"time":1794016800000}},
            {"properties":{"mag":5.9,"time":1794097800000}}
        ]}"#;
        let ex = max_mag(
            Some(4.0),
            Some("2026-11-07T00:00:00"),
            Some("2026-11-08T00:00:00"),
        );
        // Yesterday's 6.5 and tomorrow's 5.9 fall outside the UTC day.
        assert_eq!(ex.extract(body), Some(5.3));
        // Mixed ISO-string and epoch-ms timestamps in one body.
        let mixed = br#"{"features":[
            {"properties":{"mag":4.9,"time":"2026-11-07T11:35:38.000Z"}},
            {"properties":{"mag":5.5,"time":1794049663355}}
        ]}"#;
        assert_eq!(ex.extract(mixed), Some(5.5));
        // Epoch times with no window configured: everything passes.
        assert_eq!(max_mag(None, None, None).extract(body), Some(6.5));
    }

    #[test]
    fn parse_utc_secs_round_trips_with_engine_stamp() {
        // Same civil math as engine utc_stamp (tested there in reverse).
        assert_eq!(parse_utc_secs("1970-01-01T00:00:00"), Some(0));
        assert_eq!(parse_utc_secs("2026-11-07T00:00:00"), Some(1_794_009_600));
        assert_eq!(
            parse_utc_secs("2026-11-07T23:59:59"),
            Some(1_794_009_600 + 86_399)
        );
        // Fractional seconds / zone suffix ignored via 19-char prefix.
        assert_eq!(
            parse_utc_secs("2026-11-07T00:00:00.000Z"),
            Some(1_794_009_600)
        );
        assert_eq!(parse_utc_secs("garbage"), None);
        assert_eq!(parse_utc_secs("2026-13-07T00:00:00"), None);
        assert_eq!(parse_utc_secs(""), None);
    }

    #[test]
    fn json_pointer_number() {
        let ex = Extractor::JsonPointerNumber { pointer: "/tweetCount".into() };
        assert_eq!(
            ex.extract(br#"{"$schema":"x","tweetCount":21}"#),
            Some(21.0)
        );
        let nested = Extractor::JsonPointerNumber { pointer: "/data/0/count".into() };
        assert_eq!(nested.extract(br#"{"data":[{"count":7}]}"#), Some(7.0));
        assert_eq!(ex.extract(br#"{"tweetCount":"21"}"#), None); // string, not number
        assert_eq!(ex.extract(br#"{"other":1}"#), None);
        assert_eq!(ex.extract(b"not json"), None);
    }

    #[test]
    fn raw_hash_yields_no_signal() {
        assert_eq!(Extractor::RawHash.extract(br#"{"tweetCount":21}"#), None);
    }

    #[test]
    fn extractor_deserializes_from_config() {
        let raw: Extractor = serde_json::from_str(r#"{"kind":"raw_hash"}"#).unwrap();
        assert_eq!(raw, Extractor::RawHash);
        // Bare form defaults all filters off.
        let usgs: Extractor = serde_json::from_str(r#"{"kind":"usgs_geojson_max_mag"}"#).unwrap();
        assert_eq!(
            usgs,
            Extractor::UsgsGeoJsonMaxMag {
                start_time_utc: None,
                end_time_utc: None,
                min_mag: None
            }
        );
        let full: Extractor = serde_json::from_str(
            r#"{"kind":"usgs_geojson_max_mag","start_time_utc":"2026-11-07T00:00:00","end_time_utc":"2026-11-08T00:00:00","min_mag":4.0}"#,
        )
        .unwrap();
        assert_eq!(
            full,
            Extractor::UsgsGeoJsonMaxMag {
                start_time_utc: Some("2026-11-07T00:00:00".into()),
                end_time_utc: Some("2026-11-08T00:00:00".into()),
                min_mag: Some(4.0)
            }
        );
        let ptr: Extractor =
            serde_json::from_str(r#"{"kind":"json_pointer_number","pointer":"/tweetCount"}"#)
                .unwrap();
        assert_eq!(
            ptr,
            Extractor::JsonPointerNumber { pointer: "/tweetCount".into() }
        );
        // Default when omitted.
        assert_eq!(Extractor::default(), Extractor::RawHash);
    }
}