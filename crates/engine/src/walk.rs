//! Post-signal forward-walk simulation (docs/MEASUREMENT-SPEC.md
//! "Post-signal forward-walk protocol"). At each fixed offset after the
//! origin signal we sweep the LIVE ask stack for the target notional
//! (multi-level VWAP, cap price, fee) and value winning shares at $1.00
//! redemption. The verdict distinguishes `PHANTOM_EDGE` (profitable at +0 ms
//! but the quote is gone by +50/+100 ms — the MM cancel beat our transit)
//! from `EXECUTABLE_EDGE` (net profit persists at +100 ms and +250 ms with a
//! continuously healthy book).

#[derive(Debug, Clone)]
pub struct Sweep {
    /// USDC spent buying shares.
    pub spent: f64,
    /// Shares acquired.
    pub shares: f64,
    /// Volume-weighted average fill price (0 if nothing filled).
    pub avg_price: f64,
    /// The full notional was deployed.
    pub fully_filled: bool,
    /// Stopped early because the next level exceeded the cap price.
    pub cap_limited: bool,
    /// Fee charged on positive profit (fee_bps of winnings, per the
    /// source's claimed 2%-of-winnings schedule; 0 when fee_bps = 0).
    pub fee: f64,
    /// Payout if the position resolves in our favor: shares × $1 − cost − fee.
    pub net_if_win: f64,
}

/// Sweep ask levels (must be sorted ascending by price) buying up to
/// `notional` USDC, never paying more than `cap_price` per share.
pub fn sweep_asks(asks: &[(f64, f64)], notional: f64, cap_price: f64, fee_bps: f64) -> Sweep {
    let mut spent = 0.0f64;
    let mut shares = 0.0f64;
    let mut cap_limited = false;
    for &(price, size) in asks {
        if price > cap_price {
            cap_limited = true;
            break;
        }
        let room = notional - spent;
        if room <= 1e-9 {
            break;
        }
        let level_cost = size * price;
        let take_cost = room.min(level_cost);
        spent += take_cost;
        shares += take_cost / price;
    }
    let profit_pre_fee = shares - spent; // shares redeem at $1.00
    let fee = (fee_bps / 10_000.0) * profit_pre_fee.max(0.0);
    Sweep {
        fully_filled: spent >= notional - 1e-6,
        spent,
        shares,
        avg_price: if shares > 0.0 { spent / shares } else { 0.0 },
        cap_limited,
        fee,
        net_if_win: profit_pre_fee - fee,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    ExecutableEdge,
    PhantomEdge,
    NoEdge,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::ExecutableEdge => "EXECUTABLE_EDGE",
            Verdict::PhantomEdge => "PHANTOM_EDGE",
            Verdict::NoEdge => "NO_EDGE",
        }
    }
}

/// Judge a completed forward walk from its (offset_ms, net_if_win) samples.
/// Offsets follow the spec: edge must persist at +100 ms AND +250 ms with the
/// book continuously healthy to be executable; profitable at +0 ms but dead
/// by +100 ms is phantom.
pub fn judge(evs: &[(u128, f64)], min_profit: f64, healthy_all: bool) -> Verdict {
    let ev_at = |ms: u128| -> Option<f64> {
        evs.iter().find(|(o, _)| *o == ms).map(|(_, e)| *e)
    };
    match (ev_at(0), ev_at(100), ev_at(250)) {
        (Some(e0), Some(e100), Some(e250))
            if healthy_all && e0 >= min_profit && e100 >= min_profit && e250 >= min_profit =>
        {
            Verdict::ExecutableEdge
        }
        (Some(e0), Some(e100), _) if e0 >= min_profit && e100 <= 0.0 => Verdict::PhantomEdge,
        _ => Verdict::NoEdge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_single_level() {
        let s = sweep_asks(&[(0.50, 100.0)], 10.0, 0.95, 0.0);
        assert!(s.fully_filled);
        assert_eq!(s.shares, 20.0); // $10 / $0.50
        assert_eq!(s.avg_price, 0.50);
        assert_eq!(s.net_if_win, 10.0); // 20 × $1 − $10
    }

    #[test]
    fn sweep_multi_level_vwap() {
        // $10 notional: $6 at 0.60 (10 sh), $4 at 0.80 (5 sh) = 15 shares.
        let s = sweep_asks(&[(0.60, 10.0), (0.80, 50.0)], 10.0, 0.95, 0.0);
        assert!(s.fully_filled);
        assert!((s.shares - 15.0).abs() < 1e-9);
        assert!((s.avg_price - (10.0 / 15.0)).abs() < 1e-9);
        assert_eq!(s.net_if_win, 5.0); // 15 − 10
    }

    #[test]
    fn sweep_respects_cap() {
        let s = sweep_asks(&[(0.90, 5.0), (0.99, 100.0)], 20.0, 0.95, 0.0);
        assert!(s.cap_limited);
        assert!(!s.fully_filled);
        assert_eq!(s.shares, 5.0); // only the 0.90 level is ≤ cap
    }

    #[test]
    fn sweep_empty_book_no_fill() {
        let s = sweep_asks(&[], 10.0, 0.95, 0.0);
        assert_eq!(s.shares, 0.0);
        assert_eq!(s.net_if_win, 0.0);
    }

    #[test]
    fn fee_on_winnings() {
        let s = sweep_asks(&[(0.50, 100.0)], 10.0, 0.95, 200.0); // 2% of $10 profit
        assert!((s.fee - 0.20).abs() < 1e-9);
        assert!((s.net_if_win - 9.80).abs() < 1e-9);
    }

    #[test]
    fn verdict_executable() {
        let evs = vec![(0, 8.0), (25, 8.0), (50, 7.5), (100, 7.0), (250, 6.5), (500, 6.0)];
        assert_eq!(judge(&evs, 1.0, true), Verdict::ExecutableEdge);
    }

    #[test]
    fn verdict_phantom() {
        // Ask looked free at +0, MM cancel landed by +100.
        let evs = vec![(0, 8.0), (25, 4.0), (50, 0.5), (100, -0.5), (250, 0.0), (500, 0.0)];
        assert_eq!(judge(&evs, 1.0, true), Verdict::PhantomEdge);
    }

    #[test]
    fn verdict_unhealthy_downgrades() {
        let evs = vec![(0, 8.0), (25, 8.0), (50, 8.0), (100, 8.0), (250, 8.0), (500, 8.0)];
        assert_eq!(judge(&evs, 1.0, false), Verdict::NoEdge);
    }

    #[test]
    fn verdict_no_edge() {
        let evs = vec![(0, 0.2), (25, 0.2), (50, 0.1), (100, 0.0), (250, 0.0), (500, 0.0)];
        assert_eq!(judge(&evs, 1.0, true), Verdict::NoEdge);
    }
}