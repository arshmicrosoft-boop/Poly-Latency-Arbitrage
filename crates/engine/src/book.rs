//! In-memory order-book tracker rebuilt from WS `book` snapshots and
//! `price_change` deltas. Prices are stored as integer micro-prices
//! (price * 1e6) so BTreeMap ordering is exact and cheap.
//!
//! Health state machine per docs/MEASUREMENT-SPEC.md §4:
//! `Uninitialized / Healthy / SequenceGap / Disconnected / Stale`.
//! The Polymarket market channel provides no sequence numbers, so
//! `SequenceGap` is inferred when deltas arrive for a book we never received
//! a snapshot for; `Disconnected` is set by the engine on feed loss and
//! `Stale` when a healthy book goes quiet past a threshold. Races and
//! forward-walk samples are only credited from a `Healthy` (or `Stale`-but-
//! present) book.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn micro(price: f64) -> u64 {
    (price * 1_000_000.0).round() as u64
}

fn micro_to_f(m: u64) -> f64 {
    m as f64 / 1_000_000.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookHealth {
    Uninitialized,
    Healthy,
    SequenceGap,
    Disconnected,
    Stale,
}

impl BookHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            BookHealth::Uninitialized => "Uninitialized",
            BookHealth::Healthy => "Healthy",
            BookHealth::SequenceGap => "SequenceGapDetected",
            BookHealth::Disconnected => "Disconnected",
            BookHealth::Stale => "StaleFeed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BookTracker {
    bids: BTreeMap<u64, f64>,
    asks: BTreeMap<u64, f64>,
    health: BookHealth,
    last_update: Option<Instant>,
}

impl Default for BookTracker {
    fn default() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            health: BookHealth::Uninitialized,
            last_update: None,
        }
    }
}

impl BookTracker {
    pub fn health(&self) -> BookHealth {
        self.health
    }

    pub fn last_update(&self) -> Option<Instant> {
        self.last_update
    }

    /// Transition to `Stale` if healthy but quiet for longer than `max_age`.
    pub fn stale_if_quiet(&mut self, max_age: Duration) {
        if self.health == BookHealth::Healthy {
            if let Some(t) = self.last_update {
                if t.elapsed() > max_age {
                    self.health = BookHealth::Stale;
                }
            }
        }
    }

    pub fn mark_disconnected(&mut self) {
        self.health = BookHealth::Disconnected;
    }

    pub fn load_snapshot(&mut self, bids: &[(f64, f64)], asks: &[(f64, f64)]) {
        self.bids.clear();
        self.asks.clear();
        for (p, s) in bids {
            self.bids.insert(micro(*p), *s);
        }
        for (p, s) in asks {
            self.asks.insert(micro(*p), *s);
        }
        self.health = BookHealth::Healthy;
        self.last_update = Some(Instant::now());
    }

    /// size <= 0 removes the level (exchange convention for deletes).
    /// Deltas applied to an unverified book are dropped and flag
    /// `SequenceGap` — a reconnect can otherwise look like (or hide) a
    /// repricing.
    pub fn apply_change(&mut self, side: clob::ws::Side, price: f64, size: f64) {
        match self.health {
            BookHealth::Healthy | BookHealth::Stale => {}
            BookHealth::Disconnected | BookHealth::Uninitialized | BookHealth::SequenceGap => {
                self.health = BookHealth::SequenceGap;
                return;
            }
        }
        let m = match side {
            clob::ws::Side::Bid => &mut self.bids,
            clob::ws::Side::Ask => &mut self.asks,
        };
        if size <= 0.0 {
            m.remove(&micro(price));
        } else {
            m.insert(micro(price), size);
        }
        self.health = BookHealth::Healthy;
        self.last_update = Some(Instant::now());
    }

    /// (price, size) of the highest resting bid, if any.
    pub fn best_bid(&self) -> Option<(f64, f64)> {
        self.bids
            .iter()
            .next_back()
            .map(|(k, s)| (micro_to_f(*k), *s))
    }

    /// (price, size) of the lowest resting ask, if any.
    pub fn best_ask(&self) -> Option<(f64, f64)> {
        self.asks.iter().next().map(|(k, s)| (micro_to_f(*k), *s))
    }

    /// All ask levels ascending by price — input order for the forward-walk
    /// sweep simulation.
    pub fn asks_snapshot(&self) -> Vec<(f64, f64)> {
        self.asks
            .iter()
            .map(|(k, s)| (micro_to_f(*k), *s))
            .collect()
    }

    pub fn from_rest(book: &clob::Book) -> Self {
        let mut t = Self::default();
        let bids: Vec<(f64, f64)> = book.bids.iter().map(|l| (l.price, l.size)).collect();
        let asks: Vec<(f64, f64)> = book.asks.iter().map(|l| (l.price, l.size)).collect();
        t.load_snapshot(&bids, &asks);
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clob::ws::Side;

    #[test]
    fn snapshot_best_quotes() {
        let mut t = BookTracker::default();
        t.load_snapshot(&[(0.60, 100.0), (0.62, 50.0), (0.58, 200.0)], &[(0.66, 80.0), (0.64, 30.0)]);
        assert_eq!(t.best_bid().unwrap(), (0.62, 50.0));
        assert_eq!(t.best_ask().unwrap(), (0.64, 30.0));
        assert_eq!(t.health(), BookHealth::Healthy);
    }

    #[test]
    fn apply_changes() {
        let mut t = BookTracker::default();
        t.load_snapshot(&[(0.60, 100.0)], &[(0.64, 30.0)]);
        // New tighter ask appears.
        t.apply_change(Side::Ask, 0.63, 12.0);
        assert_eq!(t.best_ask().unwrap().0, 0.63);
        // That level is pulled (size 0 = delete).
        t.apply_change(Side::Ask, 0.63, 0.0);
        assert_eq!(t.best_ask().unwrap().0, 0.64);
        // Bid updates in place.
        t.apply_change(Side::Bid, 0.60, 0.0);
        assert!(t.best_bid().is_none());
    }

    #[test]
    fn empty_book() {
        let t = BookTracker::default();
        assert!(t.best_bid().is_none());
        assert!(t.best_ask().is_none());
        assert_eq!(t.health(), BookHealth::Uninitialized);
    }

    #[test]
    fn delta_before_snapshot_flags_gap() {
        let mut t = BookTracker::default();
        t.apply_change(Side::Ask, 0.5, 10.0);
        assert_eq!(t.health(), BookHealth::SequenceGap);
        assert!(t.best_ask().is_none()); // delta dropped, not applied blind
        // Snapshot resync restores health.
        t.load_snapshot(&[], &[(0.5, 10.0)]);
        assert_eq!(t.health(), BookHealth::Healthy);
    }

    #[test]
    fn disconnect_and_stale() {
        let mut t = BookTracker::default();
        t.load_snapshot(&[(0.6, 5.0)], &[(0.7, 5.0)]);
        t.mark_disconnected();
        assert_eq!(t.health(), BookHealth::Disconnected);
        t.apply_change(Side::Ask, 0.69, 1.0); // delta while disconnected → gap
        assert_eq!(t.health(), BookHealth::SequenceGap);

        let mut s = BookTracker::default();
        s.load_snapshot(&[], &[(0.7, 5.0)]);
        s.stale_if_quiet(Duration::from_secs(60)); // fresh update → not stale
        assert_eq!(s.health(), BookHealth::Healthy);
        // A never-updated (Uninitialized) book must NOT be flagged stale.
        let mut u = BookTracker::default();
        u.stale_if_quiet(Duration::from_secs(0));
        assert_eq!(u.health(), BookHealth::Uninitialized);
    }

    #[test]
    fn asks_ascending() {
        let mut t = BookTracker::default();
        t.load_snapshot(&[], &[(0.66, 80.0), (0.64, 30.0), (0.70, 10.0)]);
        let a = t.asks_snapshot();
        assert_eq!(a.iter().map(|l| l.0).collect::<Vec<_>>(), vec![0.64, 0.66, 0.70]);
    }
}