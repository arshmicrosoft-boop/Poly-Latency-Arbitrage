//! In-memory order-book tracker rebuilt from WS `book` snapshots and
//! `price_change` deltas. Prices are stored as integer micro-prices
//! (price * 1e6) so BTreeMap ordering is exact and cheap.

use std::collections::BTreeMap;

fn micro(price: f64) -> u64 {
    (price * 1_000_000.0).round() as u64
}

fn micro_to_f(m: u64) -> f64 {
    m as f64 / 1_000_000.0
}

#[derive(Debug, Default, Clone)]
pub struct BookTracker {
    bids: BTreeMap<u64, f64>,
    asks: BTreeMap<u64, f64>,
}

impl BookTracker {
    pub fn load_snapshot(&mut self, bids: &[(f64, f64)], asks: &[(f64, f64)]) {
        self.bids.clear();
        self.asks.clear();
        for (p, s) in bids {
            self.bids.insert(micro(*p), *s);
        }
        for (p, s) in asks {
            self.asks.insert(micro(*p), *s);
        }
    }

    /// size <= 0 removes the level (exchange convention for deletes).
    pub fn apply_change(&mut self, side: clob::ws::Side, price: f64, size: f64) {
        let m = match side {
            clob::ws::Side::Bid => &mut self.bids,
            clob::ws::Side::Ask => &mut self.asks,
        };
        if size <= 0.0 {
            m.remove(&micro(price));
        } else {
            m.insert(micro(price), size);
        }
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
    }
}